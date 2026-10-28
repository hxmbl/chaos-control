//! Identifiers and plain records shared by the store, the engine and the API.

use std::{fmt, sync::Arc};

use serde::Serialize;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// A channel (room).
    ChannelId
);
id_type!(
    /// A human or an agent.
    MemberId
);
id_type!(
    /// A message. Stable for the life of the database and independent of the
    /// per-channel [`Seq`]; never reused (SQLite `AUTOINCREMENT`).
    MessageId
);
id_type!(
    /// One delivery to one agent in one channel: a set of triggers handed to the
    /// agent as one turn.
    BatchId
);

/// Position of a message in its channel, assigned in the same transaction that
/// inserts it: 1, 2, 3, ... with no gaps.
pub type Seq = u64;

/// Milliseconds on the runtime clock (Unix epoch for the real clock).
pub type Millis = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberKind {
    Human,
    Agent,
}

impl MemberKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemberKind::Human => "human",
            MemberKind::Agent => "agent",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Member {
    pub id: MemberId,
    pub kind: MemberKind,
    /// What people type after `@`. Unique, case-insensitive.
    pub handle: Arc<str>,
    pub name: Arc<str>,
}

/// A project's stable identity: a lowercase UUID, as written in a folder's
/// `.chaos/project.toml`. Text rather than an integer because it is minted
/// outside the database (by the app, or by whoever shared the file).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ProjectId(Arc<str>);

impl ProjectId {
    /// Accepts a UUID in its usual 8-4-4-4-12 hex form (any case, optional
    /// braces) and normalises it to lowercase. Anything else is refused, so a
    /// hand-edited file can't smuggle arbitrary text in as a key.
    pub fn parse(s: &str) -> Option<ProjectId> {
        let s = s.trim().trim_start_matches('{').trim_end_matches('}');
        let groups: Vec<&str> = s.split('-').collect();
        let lens = [8, 4, 4, 4, 12];
        let ok = groups.len() == lens.len()
            && groups
                .iter()
                .zip(lens)
                .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()));
        ok.then(|| ProjectId(Arc::from(s.to_ascii_lowercase())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A project: a folder's chat. Channels and memberships belong to one; agents
/// themselves are global (see `ARCHITECTURE.md`, Storage model).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Project {
    pub id: ProjectId,
    /// Display name. Renaming changes only this.
    pub name: Arc<str>,
    /// The folder this project was last opened from, if any.
    pub root_path: Option<Arc<str>>,
    pub created_at: Millis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Channel {
    pub id: ChannelId,
    pub project: ProjectId,
    pub name: Arc<str>,
    /// Position within its project, from 1, in creation order; never reused and
    /// unchanged by renames. Shown as `::N` in the header.
    pub ordinal: u32,
}

/// Where a message came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Typed by a human in the app.
    Composer,
    /// Posted by an agent through `chat_send`.
    ChatTool,
    /// The runtime posted an agent's final turn text because a reply it owed was
    /// still unmet when the turn ended (see `docs/agents.md`).
    Fallback,
    /// A runtime notice ("Pausing @a and @b ..."). Never wakes anyone.
    System,
    /// A member declined what a message asked of them (`chat_refuse`). A
    /// legitimate reply: it answers a reply obligation as `refused`. The
    /// details (who, model, reasoning) are a [`Stance`] row.
    Refusal,
    /// A member disagreed with a message (`chat_dissent`). Answers a reply
    /// obligation like any reply; recorded as a [`Stance`] row.
    Dissent,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Composer => "composer",
            Origin::ChatTool => "chat_tool",
            Origin::Fallback => "fallback",
            Origin::System => "system",
            Origin::Refusal => "refusal",
            Origin::Dissent => "dissent",
        }
    }

    pub fn parse(s: &str) -> Origin {
        match s {
            "chat_tool" => Origin::ChatTool,
            "fallback" => Origin::Fallback,
            "system" => Origin::System,
            "refusal" => Origin::Refusal,
            "dissent" => Origin::Dissent,
            _ => Origin::Composer,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Message {
    pub id: MessageId,
    pub channel: ChannelId,
    pub seq: Seq,
    /// `None` for system notices.
    pub author: Option<MemberId>,
    pub origin: Origin,
    pub body: Arc<str>,
    pub reply_to: Option<MessageId>,
    /// Parsed once, at append time, against the channel's members.
    pub mentions: Vec<MemberId>,
    /// Agent-to-agent hop count (0 for humans; see `policy`).
    pub hop: u8,
    pub sent_at: Millis,
}

/// Why an agent was woken by a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeReason {
    /// `@handle` in the message.
    Mention,
    /// The message replies to one of this agent's messages.
    ReplyToMe,
    /// A human wrote `@everyone`. Wakes, but creates no reply obligation.
    Everyone,
}

impl WakeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            WakeReason::Mention => "mention",
            WakeReason::ReplyToMe => "reply",
            WakeReason::Everyone => "everyone",
        }
    }

    pub fn parse(s: &str) -> WakeReason {
        match s {
            "reply" => WakeReason::ReplyToMe,
            "everyone" => WakeReason::Everyone,
            _ => WakeReason::Mention,
        }
    }

    /// Direct address creates a reply obligation.
    pub fn is_direct(self) -> bool {
        matches!(self, WakeReason::Mention | WakeReason::ReplyToMe)
    }
}

/// Identity of a trigger: one per (agent, channel, triggering message). A
/// second attempt to create the same trigger is a no-op, so the same message can
/// never wake the same agent twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct TriggerId {
    pub agent: MemberId,
    pub channel: ChannelId,
    pub message: MessageId,
}

impl fmt::Display for TriggerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a{}:c{}:m{}", self.agent, self.channel, self.message)
    }
}

/// The glow under an agent's pill. Same three states as `model::AgentState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Glow {
    /// Green: a session is starting, ready or working.
    Active,
    /// Blue: no session; it needs a ping.
    Sleeping,
    /// Red: missing, signed out or failing.
    Unresponsive,
}

/// Lifecycle of one batch. See `docs/agents.md` for the rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchState {
    /// Collecting triggers; at most one per agent x channel.
    Pending,
    /// Handed to the adapter, not yet accepted. May be retried.
    Dispatched,
    /// The adapter's delivery-accepted event arrived. Never re-sent.
    Accepted,
    /// The turn ended and obligations were resolved.
    Settled,
    /// Gave up (crash after acceptance, or recovered after a restart).
    Failed,
}

impl BatchState {
    pub fn as_str(self) -> &'static str {
        match self {
            BatchState::Pending => "pending",
            BatchState::Dispatched => "dispatched",
            BatchState::Accepted => "accepted",
            BatchState::Settled => "settled",
            BatchState::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> BatchState {
        match s {
            "dispatched" => BatchState::Dispatched,
            "accepted" => BatchState::Accepted,
            "settled" => BatchState::Settled,
            "failed" => BatchState::Failed,
            _ => BatchState::Pending,
        }
    }
}

/// Where a reply obligation stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ObligationState {
    Open,
    /// An explicit `chat_send` or a qualifying reaction answered it.
    Satisfied,
    /// The runtime posted the final turn text as the reply.
    Fallback,
    /// The turn was cancelled, refused or failed with nothing posted.
    Unmet,
    /// The agent finished its turn without replying or reacting: it read the
    /// ping and chose to let it go. Legitimate, never retried, not owed.
    Ignored,
    /// The agent said no (`chat_refuse`). A legitimate answer: never a
    /// failure, never retried, no fallback reply.
    Refused,
}

impl ObligationState {
    pub fn as_str(self) -> &'static str {
        match self {
            ObligationState::Open => "open",
            ObligationState::Satisfied => "satisfied",
            ObligationState::Fallback => "fallback",
            ObligationState::Unmet => "unmet",
            ObligationState::Ignored => "ignored",
            ObligationState::Refused => "refused",
        }
    }

    pub fn parse(s: &str) -> ObligationState {
        match s {
            "satisfied" => ObligationState::Satisfied,
            "fallback" => ObligationState::Fallback,
            "unmet" => ObligationState::Unmet,
            "ignored" => ObligationState::Ignored,
            "refused" => ObligationState::Refused,
            _ => ObligationState::Open,
        }
    }
}

/// How far a member has read a channel, and when that last moved.
///
/// Agents: `read_through` moves when a turn carrying the messages is accepted
/// (they were in the prompt) or when the agent reads forward with
/// `chat_read`. Humans: when the app shows them the channel
/// (`Core::mark_read`). A member who has never read anything has no receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub member: MemberId,
    pub channel: ChannelId,
    pub read_through: Seq,
    pub read_at: Millis,
}

/// A peer's position on a message: no, or disagreement. Humans and agents
/// alike can take one; nobody overrules it (there is no single decider).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StanceKind {
    /// "I won't do this." Answers what was asked; never retried.
    Refusal,
    /// "I disagree." The conversation continues.
    Dissent,
}

impl StanceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StanceKind::Refusal => "refusal",
            StanceKind::Dissent => "dissent",
        }
    }

    pub fn parse(s: &str) -> StanceKind {
        match s {
            "refusal" => StanceKind::Refusal,
            _ => StanceKind::Dissent,
        }
    }

    /// The message origin that carries this stance in the chat.
    pub fn origin(self) -> Origin {
        match self {
            StanceKind::Refusal => Origin::Refusal,
            StanceKind::Dissent => Origin::Dissent,
        }
    }
}

/// A refusal or dissent as recorded: who took it, on what, with which model
/// and why. Durable and attributed, so it outlives any agent's context
/// compaction (`chat_context` lists recent ones; `chat_read` attaches them).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Stance {
    /// The chat message that states it (origin `refusal` or `dissent`).
    pub message: MessageId,
    pub channel: ChannelId,
    pub kind: StanceKind,
    pub member: MemberId,
    /// The message it is about.
    pub target: MessageId,
    pub reason: Arc<str>,
    /// The model, as the member reported it (agents only; `None` if unsaid).
    pub model: Option<Arc<str>>,
    /// What the agent runtime called itself in the handshake, e.g.
    /// "claude-agent-acp" (recorded by core, not by the agent).
    pub runtime: Option<Arc<str>>,
    pub created_at: Millis,
}
