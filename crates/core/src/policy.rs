//! Room policy: who may wake whom, and how often. Runtime-owned; adapters never
//! see any of this.
//!
//! - **Hop budget.** A message's `hop` is 0 for a human. An agent's post gets
//!   `1 + ` the hop of what woke it, where anything older than the channel's
//!   newest human message counts as 0. So any human message resets the chain.
//!   A mention in a message with `hop > max_hops` wakes nobody (it stays as
//!   unread context and the target shows "hop limit" presence).
//! - **Ping-pong damper.** Counts agent-to-agent wakes between the same two
//!   agents that alternate direction. The `max_alternations + 1`-th is
//!   suppressed, the pair is paused and one system line is posted. Plain human
//!   chatter does not reset it (it does reset hops, which is exactly the case
//!   the damper exists for); a human addressing either agent does.
//! - **Rate limit.** A token bucket per agent per channel on `chat_send`.
//! - **No `@everyone` for agents.** Stripped from agent posts at parse time.

use std::collections::HashMap;

use crate::types::{ChannelId, MemberId, Millis};

#[derive(Clone, Debug)]
pub struct Policy {
    /// Highest hop that may still wake an agent.
    pub max_hops: u8,
    /// Alternating wakes allowed between two agents before the damper trips.
    pub max_alternations: u32,
    /// `chat_send` posts per minute per agent per channel.
    pub posts_per_minute: u32,
    /// Messages of backfill an agent sees on its first wake in a channel.
    pub join_backfill: u64,
    /// Consecutive failed deliveries before an agent goes red.
    pub max_failures: u32,
    /// Backoff before retrying a delivery that was not accepted, by attempt.
    pub retry_backoff_ms: Vec<Millis>,
    /// A session with no turn for this long is closed (agent goes blue).
    pub idle_close_ms: Millis,
    /// Post a system note in the channel when a permission request is declined.
    pub note_declined_permissions: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            max_hops: 3,
            max_alternations: 4,
            posts_per_minute: 6,
            join_backfill: 20,
            max_failures: 3,
            retry_backoff_ms: vec![1_000, 5_000, 30_000],
            idle_close_ms: 10 * 60 * 1000,
            note_declined_permissions: true,
        }
    }
}

impl Policy {
    pub fn backoff(&self, attempt: u32) -> Millis {
        let ix = (attempt.max(1) as usize - 1).min(self.retry_backoff_ms.len().saturating_sub(1));
        self.retry_backoff_ms.get(ix).copied().unwrap_or(30_000)
    }
}

/// What the damper says about one agent-to-agent wake.
#[derive(Debug, PartialEq, Eq)]
pub enum Damp {
    Allow,
    /// This wake tripped the damper: suppress it and announce the pause.
    Trip,
    /// The pair is already paused: suppress quietly.
    Paused,
}

#[derive(Default, Debug)]
struct PairState {
    last_from: Option<MemberId>,
    alternations: u32,
    paused: bool,
}

/// In-memory policy state (rebuilt empty on restart, which only ever loosens it).
#[derive(Default, Debug)]
pub struct PolicyState {
    pairs: HashMap<(ChannelId, MemberId, MemberId), PairState>,
    buckets: HashMap<(MemberId, ChannelId), (f64, Millis)>,
}

fn pair_key(channel: ChannelId, a: MemberId, b: MemberId) -> (ChannelId, MemberId, MemberId) {
    if a < b {
        (channel, a, b)
    } else {
        (channel, b, a)
    }
}

impl PolicyState {
    /// Records an agent `from` waking agent `to` and says whether to allow it.
    pub fn agent_wake(
        &mut self,
        policy: &Policy,
        channel: ChannelId,
        from: MemberId,
        to: MemberId,
    ) -> Damp {
        let st = self.pairs.entry(pair_key(channel, from, to)).or_default();
        if st.paused {
            return Damp::Paused;
        }
        let alternates = st.last_from != Some(from);
        let next = if alternates {
            st.alternations + 1
        } else {
            st.alternations.max(1)
        };
        if next > policy.max_alternations {
            st.paused = true;
            return Damp::Trip;
        }
        st.alternations = next;
        st.last_from = Some(from);
        Damp::Allow
    }

    /// A human addressed `agent` in `channel`: forget its ping-pong history.
    pub fn human_addressed(&mut self, channel: ChannelId, agent: MemberId) {
        self.pairs
            .retain(|(c, a, b), _| !(*c == channel && (*a == agent || *b == agent)));
    }

    /// Takes one `chat_send` token, or says how long until one is free.
    pub fn take_post_token(
        &mut self,
        policy: &Policy,
        agent: MemberId,
        channel: ChannelId,
        now: Millis,
    ) -> Result<(), Millis> {
        let cap = policy.posts_per_minute.max(1) as f64;
        let per_ms = cap / 60_000.0;
        let (tokens, at) = self.buckets.entry((agent, channel)).or_insert((cap, now));
        *tokens = (*tokens + (now.saturating_sub(*at)) as f64 * per_ms).min(cap);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - *tokens) / per_ms).ceil() as Millis)
        }
    }
}

/// Finds `@handle` tokens. Returns lowercase handles in order of appearance,
/// without duplicates. A handle is letters, digits, `_`, `-` and `.`, and must
/// not be preceded by a word character (so `a@b.com` is not a mention).
pub fn parse_mentions(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')) {
            let start = i + 1;
            let mut end = start;
            while end < chars.len()
                && (chars[end].is_alphanumeric() || matches!(chars[end], '_' | '-' | '.'))
            {
                end += 1;
            }
            // Trailing dots are punctuation ("thanks @claude.").
            while end > start && chars[end - 1] == '.' {
                end -= 1;
            }
            if end > start {
                let h: String = chars[start..end].iter().collect::<String>().to_lowercase();
                if !out.contains(&h) {
                    out.push(h);
                }
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

pub const EVERYONE: &[&str] = &["everyone", "here", "channel"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_are_parsed_conservatively() {
        assert_eq!(parse_mentions("@claude can you"), vec!["claude"]);
        assert_eq!(
            parse_mentions("hey @Codex-Reviewer and @claude, thanks @claude."),
            vec!["codex-reviewer", "claude"]
        );
        assert!(parse_mentions("mail me at a@b.com").is_empty());
        assert!(parse_mentions("no mentions here @ all").is_empty());
    }

    #[test]
    fn damper_trips_after_four_alternations_and_resets_on_human_address() {
        let p = Policy::default();
        let mut s = PolicyState::default();
        let (c, a, b) = (ChannelId(1), MemberId(1), MemberId(2));
        assert_eq!(s.agent_wake(&p, c, a, b), Damp::Allow);
        assert_eq!(s.agent_wake(&p, c, b, a), Damp::Allow);
        assert_eq!(s.agent_wake(&p, c, a, b), Damp::Allow);
        assert_eq!(s.agent_wake(&p, c, b, a), Damp::Allow);
        assert_eq!(s.agent_wake(&p, c, a, b), Damp::Trip);
        assert_eq!(s.agent_wake(&p, c, b, a), Damp::Paused);
        s.human_addressed(c, b);
        assert_eq!(s.agent_wake(&p, c, a, b), Damp::Allow);
    }

    #[test]
    fn same_direction_repeats_are_not_ping_pong() {
        let p = Policy::default();
        let mut s = PolicyState::default();
        let (c, a, b) = (ChannelId(1), MemberId(1), MemberId(2));
        for _ in 0..10 {
            assert_eq!(s.agent_wake(&p, c, a, b), Damp::Allow);
        }
    }

    #[test]
    fn bucket_allows_six_a_minute() {
        let p = Policy::default();
        let mut s = PolicyState::default();
        let (a, c) = (MemberId(1), ChannelId(1));
        for _ in 0..6 {
            assert!(s.take_post_token(&p, a, c, 0).is_ok());
        }
        let wait = s.take_post_token(&p, a, c, 0).unwrap_err();
        assert_eq!(wait, 10_000);
        assert!(s.take_post_token(&p, a, c, 10_000).is_ok());
    }
}
