//! The delivery and wake engine: a synchronous state machine over the store.
//!
//! Inputs are posts, reactions, tool reads and adapter events, each stamped with
//! `now` from the runtime clock. Outputs are [`Effect`]s the host carries out
//! (dispatch a batch, close a session, tell the UI). Nothing here does I/O
//! beyond SQLite, so it can be driven by a fake clock and a fake transport.
//!
//! The rules (triggers, batches, acceptance, obligations, fallback) are written
//! up in `docs/agents.md`; the comments here point at the code that enforces them.

use std::{collections::HashMap, sync::Arc};

use rusqlite::Connection;

use crate::{
    adapter::{AgentError, AgentErrorKind, AgentEvent, Probe, SessionInfo, TurnOutcome},
    notify,
    policy::{self, Damp, Policy, PolicyState},
    store::{self, NewMessage, Store},
    types::*,
};

/// Something the host must do.
#[derive(Clone, Debug)]
pub enum Effect {
    /// Deliver a batch as one turn. The host opens a session first if needed and
    /// prepends `orientation` to the prompt on a fresh session.
    Dispatch(Dispatch),
    /// Close the session for agent x channel (idle).
    Close { agent: MemberId, channel: ChannelId },
    /// Tell the UI.
    Event(CoreEvent),
}

#[derive(Clone, Debug)]
pub struct Dispatch {
    pub agent: MemberId,
    pub channel: ChannelId,
    pub batch: BatchId,
    pub attempt: u32,
    pub prompt: Arc<str>,
    pub orientation: Arc<str>,
}

/// What the UI hears from core.
#[derive(Clone, Debug, PartialEq)]
pub enum CoreEvent {
    Message(Message),
    Reaction {
        message: MessageId,
        member: MemberId,
        emoji: Arc<str>,
    },
    Agent(AgentStatus),
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentStatus {
    pub agent: MemberId,
    pub channel: ChannelId,
    pub glow: Glow,
    /// One line for the Agent Status tooltip.
    pub presence: Option<Arc<str>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostError {
    Empty,
    NotMember,
    UnknownChannel,
    UnknownMessage,
    RateLimited { retry_after_ms: Millis },
    Store(String),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostError::Empty => f.write_str("empty message"),
            PostError::NotMember => f.write_str("not a member of that channel"),
            PostError::UnknownChannel => f.write_str("no such channel"),
            PostError::UnknownMessage => f.write_str("no such message"),
            PostError::RateLimited { retry_after_ms } => {
                write!(f, "rate_limited: retry after {retry_after_ms} ms")
            }
            PostError::Store(e) => write!(f, "store error: {e}"),
        }
    }
}

impl From<rusqlite::Error> for PostError {
    fn from(e: rusqlite::Error) -> Self {
        PostError::Store(e.to_string())
    }
}

pub struct PostRequest<'a> {
    pub author: MemberId,
    pub channel: ChannelId,
    pub body: &'a str,
    pub reply_to: Option<MessageId>,
    pub origin: Origin,
    pub client_request_id: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub struct Posted {
    pub message: Message,
    pub deduped: bool,
    /// Notes for the poster (e.g. "@everyone is not available to agents").
    pub notes: Vec<Arc<str>>,
}

/// Session phase per agent x channel, as the host reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    None,
    Open,
}

#[derive(Default, Debug)]
struct Rt {
    phase: Phase,
    failures: u32,
    retry_at: Option<Millis>,
    /// Set when the agent must not be retried automatically (missing, signed
    /// out, or too many failures). Cleared by a new human trigger.
    red: Option<Arc<str>>,
    /// `chat_status` text from the agent.
    status: Option<Arc<str>>,
    /// Coarse activity from the adapter during a turn.
    activity: Option<Arc<str>>,
    /// Last runtime note (declined permission, hop limit, finished silently).
    note: Option<Arc<str>>,
    idle_since: Option<Millis>,
    /// Messages the agent posted during the in-flight turn.
    sent_in_turn: Vec<(MessageId, Arc<str>)>,
    last_status: Option<(Glow, Option<Arc<str>>)>,
}

pub struct Engine {
    store: Store,
    policy: Policy,
    pstate: PolicyState,
    rt: HashMap<(MemberId, ChannelId), Rt>,
    effects: Vec<Effect>,
}

/// Emoji that answer a reply obligation. 👀 ("on it") does not: an agent that
/// reacts 👀 still owes the answer.
pub const QUALIFYING_REACTIONS: &[&str] = &["✅", "👍", "white_check_mark", "+1", "thumbsup"];

impl Engine {
    pub fn new(store: Store, policy: Policy, now: Millis) -> Result<Self, rusqlite::Error> {
        let mut engine = Engine {
            store,
            policy,
            pstate: PolicyState::default(),
            rt: HashMap::new(),
            effects: Vec::new(),
        };
        let pending = engine.store.tx(|c| store::recover(c, now))?;
        for (agent, channel) in pending {
            engine.pump(now, agent, channel);
        }
        Ok(engine)
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn conn(&self) -> &Connection {
        self.store.conn()
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    // ---------- setup ----------

    pub fn ensure_channel(&mut self, name: &str, now: Millis) -> rusqlite::Result<Channel> {
        self.store.tx(|c| store::ensure_channel(c, name, now))
    }

    pub fn ensure_member(
        &mut self,
        kind: MemberKind,
        handle: &str,
        name: &str,
    ) -> rusqlite::Result<Member> {
        self.store
            .tx(|c| store::ensure_member(c, kind, handle, name))
    }

    pub fn join(
        &mut self,
        channel: ChannelId,
        member: &Member,
        now: Millis,
    ) -> rusqlite::Result<()> {
        let backfill = self.policy.join_backfill;
        self.store
            .tx(|c| store::join(c, channel, member, backfill, now))?;
        if member.kind == MemberKind::Agent {
            self.publish_status(member.id, channel);
        }
        Ok(())
    }

    pub fn history(&self, channel: ChannelId, limit: usize) -> rusqlite::Result<Vec<Message>> {
        let head = store::head_seq(self.conn(), channel)?;
        store::messages_before(self.conn(), channel, head + 1, limit)
    }

    pub fn set_probe(&mut self, agent: MemberId, channel: ChannelId, probe: &Probe) {
        let rt = self.rt.entry((agent, channel)).or_default();
        match probe {
            Probe::Plausible => {}
            Probe::Missing { why } => rt.red = Some(why.clone()),
        }
        self.publish_status(agent, channel);
    }

    /// Current glow and presence for agent x channel.
    pub fn status(&self, agent: MemberId, channel: ChannelId) -> AgentStatus {
        let busy = store::in_flight_batch(self.conn(), agent, channel)
            .ok()
            .flatten()
            .is_some();
        let default = Rt::default();
        let rt = self.rt.get(&(agent, channel)).unwrap_or(&default);
        let failing = rt.failures >= self.policy.max_failures;
        let glow = if rt.red.is_some() || failing {
            Glow::Unresponsive
        } else if rt.phase == Phase::Open || busy || rt.retry_at.is_some() {
            Glow::Active
        } else {
            Glow::Sleeping
        };
        let presence = if let Some(red) = &rt.red {
            Some(red.clone())
        } else if busy {
            rt.status
                .clone()
                .or_else(|| {
                    rt.activity
                        .clone()
                        .map(|a| Arc::from(format!("working: {a}")))
                })
                .or_else(|| rt.note.clone())
                .or_else(|| Some(Arc::from("working")))
        } else if rt.retry_at.is_some() {
            Some(Arc::from(format!(
                "retrying delivery ({} failed {})",
                rt.failures,
                if rt.failures == 1 {
                    "attempt"
                } else {
                    "attempts"
                }
            )))
        } else {
            rt.note.clone().or_else(|| rt.status.clone())
        };
        AgentStatus {
            agent,
            channel,
            glow,
            presence,
        }
    }

    fn publish_status(&mut self, agent: MemberId, channel: ChannelId) {
        let status = self.status(agent, channel);
        let key = (status.glow, status.presence.clone());
        let rt = self.rt.entry((agent, channel)).or_default();
        if rt.last_status.as_ref() != Some(&key) {
            rt.last_status = Some(key);
            self.effects.push(Effect::Event(CoreEvent::Agent(status)));
        }
    }

    // ---------- posting ----------

    /// Appends a message and does everything that follows from it, in one
    /// transaction: dedup, rate limit, mentions, hop, obligation satisfaction,
    /// triggers and batching.
    pub fn post(&mut self, now: Millis, req: PostRequest<'_>) -> Result<Posted, PostError> {
        let body = req.body.trim();
        if body.is_empty() {
            return Err(PostError::Empty);
        }
        let author = store::member(self.conn(), req.author)?.ok_or(PostError::NotMember)?;
        store::channel(self.conn(), req.channel)?.ok_or(PostError::UnknownChannel)?;
        if !store::is_member(self.conn(), req.channel, author.id)? {
            if author.kind == MemberKind::Human {
                let backfill = self.policy.join_backfill;
                self.store
                    .tx(|c| store::join(c, req.channel, &author, backfill, now))?;
            } else {
                return Err(PostError::NotMember);
            }
        }
        if let Some(rid) = req.client_request_id
            && let Some(existing) =
                store::message_by_request(self.conn(), author.id, req.channel, rid)?
        {
            return Ok(Posted {
                message: existing,
                deduped: true,
                notes: Vec::new(),
            });
        }
        if let Some(r) = req.reply_to {
            let parent = store::message(self.conn(), r)?.ok_or(PostError::UnknownMessage)?;
            if parent.channel != req.channel {
                return Err(PostError::UnknownMessage);
            }
        }
        if author.kind == MemberKind::Agent && req.origin == Origin::ChatTool {
            self.pstate
                .take_post_token(&self.policy, author.id, req.channel, now)
                .map_err(|retry_after_ms| PostError::RateLimited { retry_after_ms })?;
        }
        let mut notes = Vec::new();
        let (mentions, everyone) = self.resolve_mentions(req.channel, &author, body, &mut notes)?;
        let cause = if author.kind == MemberKind::Agent {
            store::in_flight_batch(self.conn(), author.id, req.channel)?.map(|b| b.id)
        } else {
            None
        };
        let hop = self.hop_for(&author, req.channel, cause)?;
        let message = self.store.tx::<_, PostError>(|c| {
            Ok(store::insert_message(
                c,
                &NewMessage {
                    channel: req.channel,
                    author: Some(author.id),
                    origin: req.origin,
                    body,
                    reply_to: req.reply_to,
                    mentions: &mentions,
                    hop,
                    cause_batch: cause,
                    client_request_id: req.client_request_id,
                    sent_at: now,
                },
            )?)
        })?;
        self.effects
            .push(Effect::Event(CoreEvent::Message(message.clone())));
        if author.kind == MemberKind::Agent {
            self.on_agent_sent(&author, &message, cause)?;
        } else {
            for m in &message.mentions {
                self.pstate.human_addressed(req.channel, *m);
            }
            if let Some(parent) = message
                .reply_to
                .and_then(|r| store::message(self.conn(), r).ok().flatten())
                && let Some(a) = parent.author
            {
                self.pstate.human_addressed(req.channel, a);
            }
        }
        self.create_triggers(now, &message, &author, everyone)?;
        Ok(Posted {
            message,
            deduped: false,
            notes,
        })
    }

    /// Mentions that resolve to channel members. `@everyone`/`@here` from a
    /// human wakes every agent in the channel; from an agent it is ignored.
    fn resolve_mentions(
        &self,
        channel: ChannelId,
        author: &Member,
        body: &str,
        notes: &mut Vec<Arc<str>>,
    ) -> Result<(Vec<MemberId>, bool), PostError> {
        let members = store::members_of(self.conn(), channel)?;
        let mut out = Vec::new();
        let mut everyone = false;
        for handle in policy::parse_mentions(body) {
            if policy::EVERYONE.contains(&handle.as_str()) {
                if author.kind == MemberKind::Human {
                    everyone = true;
                } else {
                    notes.push(Arc::from(
                        "@everyone is not available to agents; mention members by handle.",
                    ));
                }
                continue;
            }
            if let Some(m) = members
                .iter()
                .find(|m| m.handle.eq_ignore_ascii_case(&handle))
                && m.id != author.id
                && !out.contains(&m.id)
            {
                out.push(m.id);
            }
        }
        Ok((out, everyone))
    }

    /// Hop of a new message (see `policy` module docs).
    fn hop_for(
        &self,
        author: &Member,
        channel: ChannelId,
        cause: Option<BatchId>,
    ) -> Result<u8, PostError> {
        if author.kind == MemberKind::Human {
            return Ok(0);
        }
        let c = self.conn();
        let last_human = store::last_human_seq(c, channel)?;
        let base = match cause {
            Some(b) => {
                let mut base = 0u8;
                for t in store::batch_triggers(c, b)? {
                    if let Some(m) = store::message(c, t.id.message)?
                        && m.seq > last_human
                    {
                        base = base.max(t.hop);
                    }
                }
                base
            }
            None => store::last_agent_message_after(c, channel, last_human)?
                .map(|m| m.hop)
                .unwrap_or(0),
        };
        Ok(base.saturating_add(1))
    }

    /// An agent posted: settle the obligation it answers (if any).
    ///
    /// Rule: a send satisfies the obligation of trigger T when it replies to T
    /// (`reply_to = T`, in the in-flight or the pending batch), or, with no
    /// `reply_to`, when T is the only open obligation of the in-flight batch.
    fn on_agent_sent(
        &mut self,
        agent: &Member,
        message: &Message,
        cause: Option<BatchId>,
    ) -> Result<(), PostError> {
        let channel = message.channel;
        let mut target = None;
        if let Some(r) = message.reply_to {
            let t = TriggerId {
                agent: agent.id,
                channel,
                message: r,
            };
            if store::obligation_state(self.conn(), t)? == Some(ObligationState::Open) {
                target = Some(t);
            }
        } else if let Some(b) = cause {
            let open = store::open_obligations(self.conn(), b)?;
            if open.len() == 1 {
                target = Some(open[0]);
            }
        }
        if let Some(t) = target {
            self.store.tx::<_, PostError>(|c| {
                store::resolve_obligation(
                    c,
                    t,
                    ObligationState::Satisfied,
                    Some(message.id),
                    "send",
                )?;
                Ok(())
            })?;
        }
        if cause.is_some() {
            let rt = self.rt.entry((agent.id, channel)).or_default();
            rt.sent_in_turn.push((message.id, message.body.clone()));
        }
        Ok(())
    }

    /// Creates triggers for every agent this message addresses and merges them
    /// into that agent's single pending batch. Untargeted messages create none.
    fn create_triggers(
        &mut self,
        now: Millis,
        message: &Message,
        author: &Member,
        everyone: bool,
    ) -> Result<(), PostError> {
        if message.origin == Origin::System {
            return Ok(());
        }
        let channel = message.channel;
        let members = store::members_of(self.conn(), channel)?;
        let reply_parent_author = match message.reply_to {
            Some(r) => store::message(self.conn(), r)?.and_then(|m| m.author),
            None => None,
        };
        let mut woken = Vec::new();
        let mut trip_notice = None;
        for target in members
            .iter()
            .filter(|m| m.kind == MemberKind::Agent && m.id != author.id)
        {
            let reason = if message.mentions.contains(&target.id) {
                WakeReason::Mention
            } else if reply_parent_author == Some(target.id) {
                WakeReason::ReplyToMe
            } else if everyone {
                WakeReason::Everyone
            } else {
                continue;
            };
            let id = TriggerId {
                agent: target.id,
                channel,
                message: message.id,
            };
            if store::trigger_exists(self.conn(), id)? {
                continue; // same message, same agent: already woken once
            }
            if author.kind == MemberKind::Agent {
                if message.hop > self.policy.max_hops {
                    let rt = self.rt.entry((target.id, channel)).or_default();
                    rt.note = Some(Arc::from(format!(
                        "@{} mentioned it, but the agent hop limit was reached; a human message resets it",
                        author.handle
                    )));
                    self.publish_status(target.id, channel);
                    continue;
                }
                match self
                    .pstate
                    .agent_wake(&self.policy, channel, author.id, target.id)
                {
                    Damp::Allow => {}
                    Damp::Trip => {
                        trip_notice = Some(format!(
                            "Pausing @{} ↔ @{}: they have pinged each other {} times in a row. Mention either to continue.",
                            author.handle, target.handle, self.policy.max_alternations
                        ));
                        continue;
                    }
                    Damp::Paused => continue,
                }
            }
            self.store.tx::<_, PostError>(|c| {
                let batch = store::pending_batch_or_create(c, target.id, channel, now)?;
                store::insert_trigger(c, id, reason, message.hop, batch.id, now)?;
                if reason.is_direct() {
                    store::insert_obligation(c, id)?;
                }
                Ok(())
            })?;
            if author.kind == MemberKind::Human {
                // A human ping is a fresh start for an agent that went red.
                let rt = self.rt.entry((target.id, channel)).or_default();
                if rt.failures >= self.policy.max_failures || rt.red.is_some() {
                    rt.failures = 0;
                    rt.red = None;
                    rt.retry_at = None;
                }
            }
            woken.push(target.id);
        }
        if let Some(text) = trip_notice {
            self.system_note(now, channel, &text)?;
        }
        for agent in woken {
            self.pump(now, agent, channel);
        }
        Ok(())
    }

    /// Re-runs trigger creation for an existing message (startup reconciliation,
    /// tests). Existing triggers are no-ops, so this never wakes twice.
    pub fn retrigger(&mut self, now: Millis, message: MessageId) -> Result<(), PostError> {
        let m = store::message(self.conn(), message)?.ok_or(PostError::UnknownMessage)?;
        let Some(author) = m
            .author
            .and_then(|a| store::member(self.conn(), a).ok().flatten())
        else {
            return Ok(());
        };
        let everyone = author.kind == MemberKind::Human
            && policy::parse_mentions(&m.body)
                .iter()
                .any(|h| policy::EVERYONE.contains(&h.as_str()));
        self.create_triggers(now, &m, &author, everyone)
    }

    pub fn system_note(
        &mut self,
        now: Millis,
        channel: ChannelId,
        text: &str,
    ) -> Result<Message, PostError> {
        let m = self.store.tx::<_, PostError>(|c| {
            Ok(store::insert_message(
                c,
                &NewMessage {
                    channel,
                    author: None,
                    origin: Origin::System,
                    body: text,
                    reply_to: None,
                    mentions: &[],
                    hop: 0,
                    cause_batch: None,
                    client_request_id: None,
                    sent_at: now,
                },
            )?)
        })?;
        self.effects
            .push(Effect::Event(CoreEvent::Message(m.clone())));
        Ok(m)
    }

    pub fn react(
        &mut self,
        now: Millis,
        member: MemberId,
        message: MessageId,
        emoji: &str,
    ) -> Result<(), PostError> {
        let m = store::message(self.conn(), message)?.ok_or(PostError::UnknownMessage)?;
        if !store::is_member(self.conn(), m.channel, member)? {
            return Err(PostError::UnknownMessage);
        }
        let added = self
            .store
            .tx(|c| store::add_reaction(c, message, member, emoji, now))?;
        if added {
            self.effects.push(Effect::Event(CoreEvent::Reaction {
                message,
                member,
                emoji: Arc::from(emoji),
            }));
        }
        if QUALIFYING_REACTIONS.contains(&emoji) {
            let t = TriggerId {
                agent: member,
                channel: m.channel,
                message,
            };
            self.store.tx::<_, PostError>(|c| {
                store::resolve_obligation(c, t, ObligationState::Satisfied, None, "react")?;
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn set_agent_status(&mut self, agent: MemberId, channel: ChannelId, text: Option<&str>) {
        let rt = self.rt.entry((agent, channel)).or_default();
        rt.status = text
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| Arc::from(t.chars().take(120).collect::<String>()));
        self.publish_status(agent, channel);
    }

    // ---------- delivery ----------

    /// Dispatches the pending batch for agent x channel if nothing is in flight
    /// and the agent is not backing off. One turn at a time; triggers that
    /// arrive meanwhile wait in the (single) pending batch.
    fn pump(&mut self, now: Millis, agent: MemberId, channel: ChannelId) {
        if let Err(e) = self.try_pump(now, agent, channel) {
            let rt = self.rt.entry((agent, channel)).or_default();
            rt.note = Some(Arc::from(format!("internal error: {e}")));
        }
        self.publish_status(agent, channel);
    }

    fn try_pump(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
    ) -> Result<(), PostError> {
        let rt = self.rt.entry((agent, channel)).or_default();
        if rt.red.is_some() || rt.failures >= self.policy.max_failures {
            return Ok(());
        }
        if rt.retry_at.is_some_and(|t| t > now) {
            return Ok(());
        }
        if store::in_flight_batch(self.conn(), agent, channel)?.is_some() {
            return Ok(());
        }
        let Some(pending) = store::pending_batch(self.conn(), agent, channel)? else {
            return Ok(());
        };
        // Everything in it already answered (e.g. by a reply sent mid-turn)?
        let triggers = store::batch_triggers(self.conn(), pending.id)?;
        let needs_delivery = triggers.iter().any(|t| {
            !t.reason.is_direct()
                || store::obligation_state(self.conn(), t.id).ok().flatten()
                    == Some(ObligationState::Open)
        });
        if !needs_delivery {
            self.store.tx::<_, PostError>(|c| {
                store::set_batch_state(
                    c,
                    pending.id,
                    BatchState::Settled,
                    now,
                    Some("answered before delivery"),
                )?;
                Ok(())
            })?;
            return Ok(());
        }
        let window_to = store::head_seq(self.conn(), channel)?;
        let attempt = self
            .store
            .tx(|c| store::dispatch_batch(c, pending.id, window_to))?;
        let prompt = notify::compose(self.conn(), agent, channel, pending.id, window_to)?;
        let orientation = notify::orientation(self.conn(), agent, channel)?;
        let rt = self.rt.entry((agent, channel)).or_default();
        rt.retry_at = None;
        rt.sent_in_turn.clear();
        rt.activity = None;
        rt.note = None;
        rt.idle_since = None;
        self.effects.push(Effect::Dispatch(Dispatch {
            agent,
            channel,
            batch: pending.id,
            attempt,
            prompt: Arc::from(prompt),
            orientation: Arc::from(orientation),
        }));
        Ok(())
    }

    /// The host opened a session.
    pub fn session_opened(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        info: &SessionInfo,
    ) {
        let _ = self.store.tx(|c| {
            store::record_session(c, agent, channel, &info.protocol, &info.native_ref, now)
        });
        let rt = self.rt.entry((agent, channel)).or_default();
        rt.phase = Phase::Open;
        rt.idle_since = Some(now);
        self.publish_status(agent, channel);
    }

    /// The host's session for agent x channel is gone (closed or exited).
    pub fn session_closed(&mut self, agent: MemberId, channel: ChannelId) {
        let rt = self.rt.entry((agent, channel)).or_default();
        rt.phase = Phase::None;
        rt.activity = None;
        rt.status = None;
        rt.idle_since = None;
        self.publish_status(agent, channel);
    }

    /// The batch could not be handed over (session failed to open, or the
    /// adapter refused the write). Not accepted, so it may be retried.
    pub fn delivery_failed(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        batch: BatchId,
        kind: AgentErrorKind,
        error: &str,
    ) {
        self.not_accepted(now, agent, channel, batch, kind, error);
    }

    pub fn agent_event(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        event: AgentEvent,
    ) {
        match event {
            AgentEvent::Accepted { turn } => self.accepted(now, agent, channel, turn),
            AgentEvent::Activity { label } => {
                self.rt.entry((agent, channel)).or_default().activity = Some(label);
                self.publish_status(agent, channel);
            }
            AgentEvent::PermissionDeclined {
                turn,
                title,
                declined_with,
            } => self.permission_declined(
                now,
                agent,
                channel,
                turn,
                &title,
                declined_with.as_deref(),
            ),
            AgentEvent::TurnEnded {
                turn,
                outcome,
                final_text,
            } => {
                self.accepted(now, agent, channel, turn);
                if let Err(e) = self.settle(now, agent, channel, turn, &outcome, &final_text) {
                    self.rt.entry((agent, channel)).or_default().note =
                        Some(Arc::from(format!("internal error: {e}")));
                }
                self.pump(now, agent, channel);
            }
            AgentEvent::TurnFailed {
                turn,
                accepted: false,
                error,
            } => self.not_accepted(now, agent, channel, turn, error.kind, &error.message),
            AgentEvent::TurnFailed {
                turn,
                accepted: true,
                error,
            } => self.failed_after_accept(now, agent, channel, turn, &error),
            AgentEvent::Exited { error } => {
                self.session_closed(agent, channel);
                if let Some(e) = error {
                    let rt = self.rt.entry((agent, channel)).or_default();
                    if rt.note.is_none() {
                        rt.note = Some(Arc::from(format!("agent exited: {e}")));
                    }
                    self.publish_status(agent, channel);
                }
            }
        }
    }

    fn accepted(&mut self, now: Millis, agent: MemberId, channel: ChannelId, turn: BatchId) {
        let Ok(Some(b)) = store::batch(self.conn(), turn) else {
            return;
        };
        if b.agent != agent || b.channel != channel || b.state != BatchState::Dispatched {
            return;
        }
        let window_to = b.window_to.unwrap_or(0);
        let _ = self.store.tx(|c| {
            store::set_batch_state(c, turn, BatchState::Accepted, now, None)?;
            store::advance_delivered(c, agent, channel, window_to)
        });
        let rt = self.rt.entry((agent, channel)).or_default();
        rt.failures = 0;
        rt.retry_at = None;
        self.publish_status(agent, channel);
    }

    fn not_accepted(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        turn: BatchId,
        kind: AgentErrorKind,
        error: &str,
    ) {
        let Ok(Some(b)) = store::batch(self.conn(), turn) else {
            return;
        };
        if b.state != BatchState::Dispatched {
            return; // accepted turns are never retried
        }
        let _ = self.store.tx(|c| store::requeue_batch(c, turn));
        let max = self.policy.max_failures;
        let rt = self.rt.entry((agent, channel)).or_default();
        rt.failures += 1;
        if kind == AgentErrorKind::Auth {
            rt.red = Some(Arc::from(format!("needs sign-in: {error}")));
            rt.retry_at = None;
        } else if rt.failures >= max {
            rt.red = Some(Arc::from(format!(
                "not responding: {} failed attempts; mention it to try again ({error})",
                rt.failures
            )));
            rt.retry_at = None;
        } else {
            rt.retry_at = Some(now + self.policy.backoff(rt.failures));
            rt.note = Some(Arc::from(format!("delivery failed: {error}")));
        }
        self.publish_status(agent, channel);
    }

    fn failed_after_accept(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        turn: BatchId,
        err: &AgentError,
    ) {
        let error: &str = &err.message;
        let Ok(Some(b)) = store::batch(self.conn(), turn) else {
            return;
        };
        if !matches!(b.state, BatchState::Accepted | BatchState::Dispatched) {
            return;
        }
        let _ = self.store.tx(|c| {
            for t in store::open_obligations(c, turn)? {
                store::resolve_obligation(c, t, ObligationState::Unmet, None, "turn_failed")?;
            }
            store::set_batch_state(c, turn, BatchState::Failed, now, Some(error))
        });
        let handle = store::member(self.conn(), agent)
            .ok()
            .flatten()
            .map(|m| m.handle)
            .unwrap_or_else(|| Arc::from("agent"));
        let auth = err.kind == AgentErrorKind::Auth;
        let note = if auth {
            format!("@{handle} needs to be signed in on this machine ({error}).")
        } else {
            format!("@{handle} stopped before finishing ({error}). Mention it again to retry.")
        };
        let _ = self.system_note(now, channel, &note);
        let max = self.policy.max_failures;
        let rt = self.rt.entry((agent, channel)).or_default();
        rt.failures += 1;
        rt.sent_in_turn.clear();
        if auth {
            // Never retried: it can't succeed until someone signs in.
            rt.red = Some(Arc::from(format!("needs sign-in: {error}")));
        } else if rt.failures >= max {
            rt.red = Some(Arc::from(format!(
                "not responding: {} failures ({error})",
                rt.failures
            )));
        } else {
            rt.retry_at = Some(now + self.policy.backoff(rt.failures));
        }
        self.publish_status(agent, channel);
        self.pump(now, agent, channel);
    }

    fn permission_declined(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        turn: Option<BatchId>,
        title: &str,
        declined_with: Option<&str>,
    ) {
        let rt = self.rt.entry((agent, channel)).or_default();
        rt.note = Some(Arc::from(format!("wanted to run {title}, declined")));
        self.publish_status(agent, channel);
        if self.policy.note_declined_permissions {
            let handle = store::member(self.conn(), agent)
                .ok()
                .flatten()
                .map(|m| m.handle)
                .unwrap_or_else(|| Arc::from("agent"));
            // Who woke it: the follow-up is to ask them instead of declining.
            let woken_by = turn
                .map(|t| self.woken_by(t))
                .unwrap_or_default()
                .join(", ");
            let suffix = if woken_by.is_empty() {
                String::new()
            } else {
                format!(" (woken by {woken_by})")
            };
            let how = declined_with
                .map(|o| format!(" with its own \"{o}\" option"))
                .unwrap_or_default();
            let _ = self.system_note(
                now,
                channel,
                &format!(
                    "@{handle} asked to run {title}; Chaos Control declined{how}. Approvals aren't supported yet{suffix}."
                ),
            );
        }
    }

    /// Handles of the humans whose messages woke this batch.
    pub fn woken_by(&self, batch: BatchId) -> Vec<String> {
        let mut out = Vec::new();
        for t in store::batch_triggers(self.conn(), batch).unwrap_or_default() {
            if let Ok(Some(m)) = store::message(self.conn(), t.id.message)
                && let Some(a) = m.author
                && let Ok(Some(who)) = store::member(self.conn(), a)
                && who.kind == MemberKind::Human
            {
                let h = format!("@{}", who.handle);
                if !out.contains(&h) {
                    out.push(h);
                }
            }
        }
        out
    }

    /// Turn end: resolve every still-open obligation of the batch.
    ///
    /// - Completed with final text: post it **once**, as a fallback reply to the
    ///   newest open trigger, and mark every open obligation of the batch
    ///   `fallback` by that one message. If the agent already posted exactly
    ///   that text this turn, nothing is posted and they count as satisfied.
    /// - Completed with no text, or any other outcome: `unmet`.
    ///
    /// Obligations already satisfied by a send or reaction are untouched (the
    /// fallback only ever looks at `open` rows, inside one transaction), so a
    /// fallback can never duplicate a reply that was already sent.
    fn settle(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        turn: BatchId,
        outcome: &TurnOutcome,
        final_text: &str,
    ) -> Result<(), PostError> {
        let Some(b) = store::batch(self.conn(), turn)? else {
            return Ok(());
        };
        if b.agent != agent || b.channel != channel || b.state != BatchState::Accepted {
            return Ok(());
        }
        let open = store::open_obligations(self.conn(), turn)?;
        let text = final_text.trim();
        let sent = std::mem::take(&mut self.rt.entry((agent, channel)).or_default().sent_in_turn);
        let mut fallback_message = None;
        let outcome_label = format!("{outcome:?}");
        if !open.is_empty() && *outcome == TurnOutcome::Completed && !text.is_empty() {
            if let Some((echo, _)) = sent.iter().find(|(_, body)| body.trim() == text) {
                let echo = *echo;
                self.store.tx::<_, PostError>(|c| {
                    for t in &open {
                        store::resolve_obligation(
                            c,
                            *t,
                            ObligationState::Satisfied,
                            Some(echo),
                            "send_echo",
                        )?;
                    }
                    Ok(())
                })?;
            } else {
                let author = store::member(self.conn(), agent)?.ok_or(PostError::NotMember)?;
                let target = *open.last().expect("non-empty");
                let mut notes = Vec::new();
                let (mentions, _) = self.resolve_mentions(channel, &author, text, &mut notes)?;
                let hop = self.hop_for(&author, channel, Some(turn))?;
                let m = self.store.tx::<_, PostError>(|c| {
                    // Re-check inside the transaction: only still-open rows count.
                    let still_open = store::open_obligations(c, turn)?;
                    if still_open.is_empty() {
                        return Ok(None);
                    }
                    let m = store::insert_message(
                        c,
                        &NewMessage {
                            channel,
                            author: Some(agent),
                            origin: Origin::Fallback,
                            body: text,
                            reply_to: Some(target.message),
                            mentions: &mentions,
                            hop,
                            cause_batch: Some(turn),
                            client_request_id: None,
                            sent_at: now,
                        },
                    )?;
                    for t in still_open {
                        store::resolve_obligation(
                            c,
                            t,
                            ObligationState::Fallback,
                            Some(m.id),
                            "fallback",
                        )?;
                    }
                    Ok(Some(m))
                })?;
                fallback_message = m.map(|m| (m, author));
            }
        } else if !open.is_empty() {
            self.store.tx::<_, PostError>(|c| {
                for t in &open {
                    store::resolve_obligation(c, *t, ObligationState::Unmet, None, "no_reply")?;
                }
                Ok(())
            })?;
            let rt = self.rt.entry((agent, channel)).or_default();
            rt.note = Some(Arc::from(if *outcome == TurnOutcome::Completed {
                "finished without replying"
            } else {
                "turn ended without a reply"
            }));
        }
        self.store.tx::<_, PostError>(|c| {
            store::set_batch_state(c, turn, BatchState::Settled, now, Some(&outcome_label))?;
            Ok(())
        })?;
        let rt = self.rt.entry((agent, channel)).or_default();
        rt.activity = None;
        rt.status = None;
        rt.idle_since = Some(now);
        if let Some((m, author)) = fallback_message {
            self.effects
                .push(Effect::Event(CoreEvent::Message(m.clone())));
            self.create_triggers(now, &m, &author, false)?;
        }
        Ok(())
    }

    // ---------- time ----------

    pub fn next_deadline(&self) -> Option<Millis> {
        self.rt
            .values()
            .flat_map(|rt| {
                let idle = rt
                    .idle_since
                    .filter(|_| rt.phase == Phase::Open)
                    .map(|t| t + self.policy.idle_close_ms);
                [rt.retry_at, idle]
            })
            .flatten()
            .min()
    }

    pub fn tick(&mut self, now: Millis) {
        let keys: Vec<_> = self.rt.keys().copied().collect();
        for (agent, channel) in keys {
            let due_retry = self.rt[&(agent, channel)]
                .retry_at
                .is_some_and(|t| t <= now);
            if due_retry {
                self.rt.get_mut(&(agent, channel)).expect("key").retry_at = None;
                self.pump(now, agent, channel);
            }
            let rt = &self.rt[&(agent, channel)];
            let idle_due = rt.phase == Phase::Open
                && rt
                    .idle_since
                    .is_some_and(|t| t + self.policy.idle_close_ms <= now);
            if idle_due {
                // Busy sessions are not idle; the clock restarts at turn end.
                self.rt.get_mut(&(agent, channel)).expect("key").idle_since = None;
                let busy = store::in_flight_batch(self.conn(), agent, channel)
                    .ok()
                    .flatten()
                    .is_some();
                if !busy {
                    self.effects.push(Effect::Close { agent, channel });
                }
            }
        }
    }

    // ---------- reads for tools ----------

    pub fn advance_read(&mut self, agent: MemberId, channel: ChannelId, seq: Seq) {
        let _ = self
            .store
            .tx(|c| store::advance_read(c, agent, channel, seq));
    }
}
