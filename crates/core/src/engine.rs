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
    permission::{self, PermissionRequest, RunsIn},
    policy::{self, Damp, Policy, PolicyState},
    reaction::{self, PingAnswers, PingAnswersError},
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
    /// A reaction was added (`added`) or removed. Never a wake: nobody is
    /// notified or delivered anything because of it.
    Reaction {
        channel: ChannelId,
        message: MessageId,
        member: MemberId,
        emoji: Arc<str>,
        added: bool,
    },
    Agent(AgentStatus),
    /// A member's read receipt moved forward.
    Read(Receipt),
    /// A permission request was sent, answered, auto-approved or used.
    Permission(PermissionRequest),
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
    /// Not something that can be a reaction (a word, blank, too long).
    InvalidEmoji,
    RateLimited {
        retry_after_ms: Millis,
    },
    Store(String),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostError::Empty => f.write_str("empty message"),
            PostError::NotMember => f.write_str("not a member of that channel"),
            PostError::UnknownChannel => f.write_str("no such channel"),
            PostError::UnknownMessage => f.write_str("no such message"),
            PostError::InvalidEmoji => {
                f.write_str("invalid_emoji: send one emoji (or a :shortcode: like :+1:)")
            }
            PostError::RateLimited { retry_after_ms } => {
                write!(f, "rate_limited: retry after {retry_after_ms} ms")
            }
            PostError::Store(e) => write!(f, "store error: {e}"),
        }
    }
}

/// Why a setting was not changed.
#[derive(Debug)]
pub enum SettingError {
    Invalid(PingAnswersError),
    UnknownProject,
    Store(String),
}

impl std::fmt::Display for SettingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingError::Invalid(e) => e.fmt(f),
            SettingError::UnknownProject => f.write_str("no such project"),
            SettingError::Store(e) => write!(f, "store error: {e}"),
        }
    }
}

impl std::error::Error for SettingError {}

impl From<PingAnswersError> for SettingError {
    fn from(e: PingAnswersError) -> Self {
        SettingError::Invalid(e)
    }
}

impl From<rusqlite::Error> for SettingError {
    fn from(e: rusqlite::Error) -> Self {
        SettingError::Store(e.to_string())
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

/// A refusal or dissent (see [`Engine::take_stance`]).
pub struct StanceRequest<'a> {
    pub author: MemberId,
    pub channel: ChannelId,
    pub kind: StanceKind,
    /// The message the stance is about. Must be in `channel`.
    pub target: MessageId,
    /// Why, in the member's own words. Posted as the message body.
    pub reason: &'a str,
    /// The model, as the member reports it.
    pub model: Option<&'a str>,
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
    /// What the agent runtime called itself in the last handshake.
    runtime: Option<Arc<str>>,
    last_status: Option<(Glow, Option<Arc<str>>)>,
}

pub struct Engine {
    pub(crate) store: Store,
    policy: Policy,
    pstate: PolicyState,
    rt: HashMap<(MemberId, ChannelId), Rt>,
    pub(crate) effects: Vec<Effect>,
    /// Where each agent runs (see `permission`); absent means the host.
    pub(crate) runs_in: HashMap<MemberId, RunsIn>,
    /// Per-agent ping-answer lists from `agents.json`; absent means the
    /// project's setting, then the default (see [`Engine::ping_answers`]).
    ping_answers: HashMap<MemberId, PingAnswers>,
}

/// What [`Engine::react`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reacted {
    /// The emoji as stored (normalised: `:+1:` is 👍).
    pub emoji: Arc<str>,
    /// Whether anything changed (adding an emoji already there, or removing
    /// one that isn't, is a no-op).
    pub changed: bool,
}

impl Engine {
    pub fn new(store: Store, policy: Policy, now: Millis) -> Result<Self, rusqlite::Error> {
        let mut engine = Engine {
            store,
            policy,
            pstate: PolicyState::default(),
            rt: HashMap::new(),
            effects: Vec::new(),
            runs_in: HashMap::new(),
            ping_answers: HashMap::new(),
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

    /// See [`store::ensure_project`]: creates the project or refreshes its
    /// name and last-seen folder. Grants nobody anything.
    pub fn ensure_project(
        &mut self,
        id: &ProjectId,
        name: &str,
        root: Option<&str>,
        now: Millis,
    ) -> rusqlite::Result<Project> {
        self.store
            .tx(|c| store::ensure_project(c, id, name, root, now))
    }

    /// See [`store::adopt_import`].
    pub fn adopt_import(
        &mut self,
        id: &ProjectId,
        name: &str,
        root: &str,
    ) -> rusqlite::Result<Option<Project>> {
        self.store.tx(|c| store::adopt_import(c, id, name, root))
    }

    /// See [`store::forget_empty_project`]. Also drops any runtime state for
    /// the deleted channels. Returns whether the project went.
    pub fn forget_empty_project(&mut self, id: &ProjectId) -> rusqlite::Result<bool> {
        let Some(gone) = self.store.tx(|c| store::forget_empty_project(c, id))? else {
            return Ok(false);
        };
        self.rt.retain(|(_, ch), _| !gone.contains(ch));
        Ok(true)
    }

    /// The channel `name` in `project`, created if new. The project must exist.
    pub fn ensure_channel(
        &mut self,
        project: &ProjectId,
        name: &str,
        now: Millis,
    ) -> rusqlite::Result<Channel> {
        self.store
            .tx(|c| store::ensure_channel(c, project, name, now))
    }

    /// Makes `member` a member of `project`, which is what lets it join the
    /// project's channels.
    pub fn join_project(
        &mut self,
        project: &ProjectId,
        member: MemberId,
        now: Millis,
    ) -> rusqlite::Result<bool> {
        self.store
            .tx(|c| store::join_project(c, project, member, now))
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
        // A pending permission request shows as such (never "blocked": the
        // agent may well be doing something else meanwhile).
        let presence = match rt.red {
            Some(_) => presence,
            None => permission::pending_presence(self.conn(), agent, channel).or(presence),
        };
        AgentStatus {
            agent,
            channel,
            glow,
            presence,
        }
    }

    pub(crate) fn publish_status(&mut self, agent: MemberId, channel: ChannelId) {
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
        let channel = store::channel(self.conn(), req.channel)?.ok_or(PostError::UnknownChannel)?;
        if !store::is_member(self.conn(), req.channel, author.id)? {
            // A human posting joins the channel, but only inside a project
            // they already belong to: posting never grants project access.
            if author.kind == MemberKind::Human
                && store::is_project_member(self.conn(), &channel.project, author.id)?
            {
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
        if author.kind == MemberKind::Agent
            && matches!(
                req.origin,
                Origin::ChatTool | Origin::Refusal | Origin::Dissent
            )
        {
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
            // A human has read what they just sent (and so everything before
            // it they could see). Never for agents: their read cursor decides
            // what their next notification calls new.
            self.advance_read(now, author.id, req.channel, message.seq);
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
            // A refusal is an answer like any other: it closes the obligation
            // (as `refused`), so there is no fallback and no retry.
            let (state, kind) = match message.origin {
                Origin::Refusal => (ObligationState::Refused, "refusal"),
                Origin::Dissent => (ObligationState::Satisfied, "dissent"),
                _ => (ObligationState::Satisfied, "send"),
            };
            self.store.tx::<_, PostError>(|c| {
                store::resolve_obligation(c, t, state, Some(message.id), kind)?;
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

    /// Adds (`add`) or removes `member`'s `emoji` on `message`. Idempotent:
    /// adding twice or removing twice changes nothing and emits nothing.
    ///
    /// A reaction never wakes anyone: no trigger, no batch, no delivery, not
    /// even to the message's author. The only side effect beyond the store is
    /// on the reactor's own reply obligation: adding an emoji that answers a
    /// ping for this member ([`Engine::ping_answers`]: ✅ and 👍 unless
    /// configured, never 👀) to a message the reactor owes a reply closes that
    /// obligation as handled (`satisfied`, kind `react`). Any other emoji is
    /// recorded but leaves the obligation open for the turn-end rules.
    /// Removing a reaction later doesn't reopen it.
    pub fn react(
        &mut self,
        now: Millis,
        member: MemberId,
        message: MessageId,
        emoji: &str,
        add: bool,
    ) -> Result<Reacted, PostError> {
        let emoji = reaction::normalize(emoji).ok_or(PostError::InvalidEmoji)?;
        let m = store::message(self.conn(), message)?.ok_or(PostError::UnknownMessage)?;
        if !store::is_member(self.conn(), m.channel, member)? {
            return Err(PostError::UnknownMessage);
        }
        let answers = self.ping_answers(member, m.channel);
        let changed = self.store.tx::<_, PostError>(|c| {
            let changed = if add {
                store::add_reaction(c, message, m.channel, member, &emoji, now)?
            } else {
                store::remove_reaction(c, message, member, &emoji)?
            };
            if add && answers.answers(&emoji) {
                let t = TriggerId {
                    agent: member,
                    channel: m.channel,
                    message,
                };
                store::resolve_obligation(c, t, ObligationState::Satisfied, None, "react")?;
            }
            Ok(changed)
        })?;
        if changed {
            self.effects.push(Effect::Event(CoreEvent::Reaction {
                channel: m.channel,
                message,
                member,
                emoji: emoji.clone(),
                added: add,
            }));
        }
        Ok(Reacted { emoji, changed })
    }

    /// Which reactions answer a ping for `agent` in `channel`: the agent's own
    /// list (`agents.json`), else the channel's project setting, else the
    /// default (✅, 👍). A stored value that no longer parses counts as unset.
    pub fn ping_answers(&self, agent: MemberId, channel: ChannelId) -> PingAnswers {
        if let Some(own) = self.ping_answers.get(&agent) {
            return own.clone();
        }
        store::channel(self.conn(), channel)
            .ok()
            .flatten()
            .and_then(|ch| self.project_ping_answers(&ch.project).ok().flatten())
            .unwrap_or_default()
    }

    /// Sets (`Some`) or clears an agent's own ping-answer list, which wins
    /// over the project's.
    pub fn set_agent_ping_answers(&mut self, agent: MemberId, answers: Option<PingAnswers>) {
        match answers {
            Some(a) => {
                self.ping_answers.insert(agent, a);
            }
            None => {
                self.ping_answers.remove(&agent);
            }
        }
    }

    /// The project's ping-answer setting, or `None` when it uses the default.
    pub fn project_ping_answers(
        &self,
        project: &ProjectId,
    ) -> Result<Option<PingAnswers>, rusqlite::Error> {
        let Some(raw) = store::project_setting(self.conn(), project, reaction::PING_ANSWERS_KEY)?
        else {
            return Ok(None);
        };
        Ok(serde_json::from_str::<Vec<String>>(&raw)
            .ok()
            .and_then(|list| PingAnswers::parse(&list).ok()))
    }

    /// Validates and stores the project's ping-answer list (`None` clears it,
    /// back to the default). Junk is rejected whole, naming every bad entry;
    /// nothing is written then. Returns what is now in effect.
    pub fn set_project_ping_answers<S: AsRef<str>>(
        &mut self,
        now: Millis,
        project: &ProjectId,
        entries: Option<&[S]>,
    ) -> Result<PingAnswers, SettingError> {
        let parsed = entries.map(PingAnswers::parse).transpose()?;
        let value = parsed
            .as_ref()
            .map(|p| serde_json::to_string(&p.to_list()).expect("strings serialise"));
        if store::project(self.conn(), project)?.is_none() {
            return Err(SettingError::UnknownProject);
        }
        store::set_project_setting(
            self.conn(),
            project,
            reaction::PING_ANSWERS_KEY,
            value.as_deref(),
            now,
        )?;
        Ok(parsed.unwrap_or_default())
    }

    /// A member refuses or dissents from `target`: posts `reason` as a reply
    /// to it (origin `refusal` / `dissent`) and records the stance with who,
    /// what, the model and the runtime. Goes through [`Engine::post`], so
    /// membership, rate limits, mentions, hops and obligations all apply; a
    /// refusal answers the author's reply obligation on `target` as
    /// `refused`. Nobody is asked to overrule it.
    pub fn take_stance(
        &mut self,
        now: Millis,
        req: StanceRequest<'_>,
    ) -> Result<Posted, PostError> {
        let reason = req.reason.trim();
        if reason.is_empty() {
            return Err(PostError::Empty);
        }
        let posted = self.post(
            now,
            PostRequest {
                author: req.author,
                channel: req.channel,
                body: reason,
                reply_to: Some(req.target),
                origin: req.kind.origin(),
                client_request_id: req.client_request_id,
            },
        )?;
        let runtime = self
            .rt
            .get(&(req.author, req.channel))
            .and_then(|rt| rt.runtime.clone());
        let model = req
            .model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| Arc::from(m.chars().take(80).collect::<String>()));
        let stance = Stance {
            message: posted.message.id,
            channel: req.channel,
            kind: req.kind,
            member: req.author,
            target: req.target,
            reason: Arc::from(reason),
            model,
            runtime,
            created_at: now,
        };
        self.store.tx(|c| store::insert_stance(c, &stance))?;
        if req.kind == StanceKind::Refusal && !posted.deduped {
            let target_seq = store::message(self.conn(), req.target)?.map(|m| m.seq);
            // Agents only (humans have no runtime state or presence).
            if let Some(rt) = self.rt.get_mut(&(req.author, req.channel)) {
                rt.note = Some(Arc::from(match target_seq {
                    Some(seq) => format!("declined #{seq}"),
                    None => "declined a request".to_string(),
                }));
                self.publish_status(req.author, req.channel);
            }
        }
        Ok(posted)
    }

    pub fn set_agent_status(&mut self, agent: MemberId, channel: ChannelId, text: Option<&str>) {
        let rt = self.rt.entry((agent, channel)).or_default();
        rt.status = text
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| Arc::from(t.chars().take(120).collect::<String>()));
        self.publish_status(agent, channel);
    }

    /// Sets the agent's runtime note (its presence when idle).
    pub(crate) fn rt_note(&mut self, agent: MemberId, channel: ChannelId, note: String) {
        self.rt.entry((agent, channel)).or_default().note = Some(Arc::from(note));
        self.publish_status(agent, channel);
    }

    /// A human turned to this agent: clear a red state so it can be tried again.
    pub(crate) fn human_fresh_start(&mut self, agent: MemberId, channel: ChannelId) {
        let max = self.policy.max_failures;
        let rt = self.rt.entry((agent, channel)).or_default();
        if rt.failures >= max || rt.red.is_some() {
            rt.failures = 0;
            rt.red = None;
            rt.retry_at = None;
        }
    }

    // ---------- delivery ----------

    /// Dispatches the pending batch for agent x channel if nothing is in flight
    /// and the agent is not backing off. One turn at a time; triggers that
    /// arrive meanwhile wait in the (single) pending batch.
    pub(crate) fn pump(&mut self, now: Millis, agent: MemberId, channel: ChannelId) {
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
        let answers = self.ping_answers(agent, channel);
        let prompt = notify::compose(self.conn(), agent, channel, pending.id, window_to, &answers)?;
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
        rt.runtime = info.agent_name.clone();
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
        let read = self.store.tx(|c| {
            store::set_batch_state(c, turn, BatchState::Accepted, now, None)?;
            store::advance_delivered(c, agent, channel, window_to, now)
        });
        if read.unwrap_or(false) {
            self.publish_receipt(agent, channel);
        }
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
            // A completed turn with nothing to say is the agent choosing to let
            // the ping go: `ignored`, which is fine. Anything else (cancelled,
            // refused, cut short) is `unmet`. A trigger the agent put 👀 on
            // (an answering emoji closed it when it was added; this also
            // catches one the config made answering since) was handled, not
            // ignored. Any other reaction doesn't count: still `ignored`.
            let completed = *outcome == TurnOutcome::Completed;
            let answers = self.ping_answers(agent, channel);
            let mut ignored_any = false;
            self.store.tx::<_, PostError>(|c| {
                for t in &open {
                    let (state, kind) = if !completed {
                        (ObligationState::Unmet, "no_reply")
                    } else if store::reactions_by(c, t.message, agent)?
                        .iter()
                        .any(|e| reaction::HOLDS_OBLIGATION.contains(&&**e) || answers.answers(e))
                    {
                        (ObligationState::Satisfied, "react")
                    } else {
                        ignored_any = true;
                        (ObligationState::Ignored, "ignored")
                    };
                    store::resolve_obligation(c, *t, state, None, kind)?;
                }
                Ok(())
            })?;
            let rt = self.rt.entry((agent, channel)).or_default();
            if !completed {
                rt.note = Some(Arc::from("turn ended without a reply"));
            } else if ignored_any {
                rt.note = Some(Arc::from("read it and let it go"));
            }
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

    /// Moves `member`'s read receipt in `channel` forward to `seq` (clamped to
    /// the channel head; never backwards). Agents read forward with
    /// `chat_read`; humans when the app shows them the channel.
    pub fn advance_read(&mut self, now: Millis, member: MemberId, channel: ChannelId, seq: Seq) {
        let head = store::head_seq(self.conn(), channel).unwrap_or(0);
        let seq = seq.min(head);
        if seq == 0 {
            return;
        }
        let moved = self
            .store
            .tx(|c| store::advance_read(c, member, channel, seq, now));
        if moved.unwrap_or(false) {
            self.publish_receipt(member, channel);
        }
    }

    /// Read receipts in `channel` (see [`Receipt`]).
    pub fn receipts(&self, channel: ChannelId) -> rusqlite::Result<Vec<Receipt>> {
        store::receipts(self.conn(), channel)
    }

    fn publish_receipt(&mut self, member: MemberId, channel: ChannelId) {
        let Ok(cursor) = store::cursor(self.conn(), member, channel) else {
            return;
        };
        if let Some(read_at) = cursor.read_at {
            self.effects.push(Effect::Event(CoreEvent::Read(Receipt {
                member,
                channel,
                read_through: cursor.read_through,
                read_at,
            })));
        }
    }
}
