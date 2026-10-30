//! The boundary between the runtime and an agent protocol.
//!
//! The runtime reads [`Caps`] from a live session and never branches on which
//! adapter (or brand) it is talking to. Adapters translate their protocol into
//! [`AgentEvent`]s and hold no room policy: no dampers, no obligations, no
//! retries. Those all live in the engine.
//!
//! # Delivery contract (every adapter)
//!
//! - [`AgentSession::deliver`] hands one prompt (one batch) to the agent and
//!   returns once it is written to the transport. An `Err` means it was never
//!   handed over; the engine may retry.
//! - After an `Ok`, the adapter emits exactly one terminal event for that turn:
//!   [`AgentEvent::TurnEnded`] or [`AgentEvent::TurnFailed`].
//! - Before the terminal event it emits [`AgentEvent::Accepted`] at most once,
//!   at its protocol's *acceptance point*. `TurnEnded` implies acceptance (the
//!   adapter emits `Accepted` first if it had not yet). `TurnFailed` carries
//!   whether acceptance happened. The engine retries only turns that failed
//!   before acceptance, so the acceptance point must never come too early.
//! - Each adapter documents its acceptance point (see `acp.rs`).

use std::{fmt, path::PathBuf, sync::Arc};

use futures::future::BoxFuture;

use crate::harness::ModelInfo;
use crate::permission::PermissionGate;
use crate::types::{BatchId, ChannelId, MemberId};

/// Capabilities negotiated for one live session.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps(u32);

impl Caps {
    /// Accepts our chat MCP server over HTTP.
    pub const CHAT_TOOLS_HTTP: Caps = Caps(1 << 0);
    /// Accepts a stdio MCP server (the `mcp-bridge`).
    pub const CHAT_TOOLS_STDIO: Caps = Caps(1 << 1);
    /// Can take new input into a running turn. Never assumed for ACP.
    pub const STEER: Caps = Caps(1 << 2);
    /// Queues a prompt sent while busy. Never assumed for ACP.
    pub const QUEUE: Caps = Caps(1 << 3);
    /// Can cancel a running turn.
    pub const INTERRUPT: Caps = Caps(1 << 4);
    /// Can reopen a native session after the process exits.
    pub const RESUME: Caps = Caps(1 << 5);
    /// Takes instructions at session start.
    pub const INSTRUCTIONS: Caps = Caps(1 << 6);
    /// May keep working after a turn ends (ACP v2 draft background work).
    pub const BACKGROUND_WORK: Caps = Caps(1 << 7);

    pub const fn empty() -> Caps {
        Caps(0)
    }

    pub const fn union(self, other: Caps) -> Caps {
        Caps(self.0 | other.0)
    }

    pub const fn contains(self, other: Caps) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Caps) {
        self.0 |= other.0;
    }
}

impl std::ops::BitOr for Caps {
    type Output = Caps;
    fn bitor(self, rhs: Caps) -> Caps {
        self.union(rhs)
    }
}

impl fmt::Debug for Caps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = [
            (Caps::CHAT_TOOLS_HTTP, "chat_tools_http"),
            (Caps::CHAT_TOOLS_STDIO, "chat_tools_stdio"),
            (Caps::STEER, "steer"),
            (Caps::QUEUE, "queue"),
            (Caps::INTERRUPT, "interrupt"),
            (Caps::RESUME, "resume"),
            (Caps::INSTRUCTIONS, "instructions"),
            (Caps::BACKGROUND_WORK, "background_work"),
        ];
        let set: Vec<_> = names
            .iter()
            .filter(|(c, _)| self.contains(*c))
            .map(|(_, n)| *n)
            .collect();
        write!(f, "Caps({})", set.join("|"))
    }
}

/// What a live session turned out to be.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub caps: Caps,
    /// e.g. "acp/v1", "acp/v2-draft".
    pub protocol: Arc<str>,
    /// Agent-side session id, kept for resume.
    pub native_ref: Arc<str>,
    /// What the agent calls itself (`agentInfo` in ACP).
    pub agent_name: Option<Arc<str>>,
    /// Vendor extensions the agent advertised that we do not use (informational).
    pub unused_extensions: Vec<Arc<str>>,
    /// Which chat-tools transport was injected.
    pub chat_tools: ChatToolsTransport,
    /// The model the session runs, as the harness named it (`None` when it
    /// did not say).
    pub model: Option<Arc<str>>,
    /// The models the harness offered for this session (ACP session config
    /// options, Codex `model/list`). Empty when it offered none.
    pub models: Vec<ModelInfo>,
    /// Why the member's model could not be applied, if it could not (the
    /// session then runs the harness default). Shown as presence.
    pub model_note: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatToolsTransport {
    Http,
    StdioBridge,
    None,
}

/// Result of a cheap, side-effect-free availability check: no process spawn, no
/// session, no sign-in. It only says an agent is *plausibly* available; the
/// protocol version and capabilities are settled by `open`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    Plausible,
    Missing { why: Arc<str> },
}

/// Where the agent reaches our chat tools.
#[derive(Clone, Debug)]
pub struct ChatToolsEndpoint {
    /// `http://127.0.0.1:<port>/mcp`
    pub url: String,
    /// Bearer token scoped to this session; revoked when it closes.
    pub token: String,
    /// How to start the stdio bridge (`chaos-control mcp-bridge`), if available.
    pub bridge: Option<BridgeCommand>,
}

#[derive(Clone, Debug)]
pub struct BridgeCommand {
    pub command: PathBuf,
    pub args: Vec<String>,
}

/// Where adapter events go. Cheap to clone.
#[derive(Clone)]
pub struct EventSink(Arc<dyn Fn(AgentEvent) + Send + Sync>);

impl EventSink {
    pub fn new(f: impl Fn(AgentEvent) + Send + Sync + 'static) -> Self {
        EventSink(Arc::new(f))
    }

    pub fn emit(&self, event: AgentEvent) {
        (self.0)(event)
    }
}

impl fmt::Debug for EventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EventSink")
    }
}

pub struct OpenSession {
    pub agent: MemberId,
    pub channel: ChannelId,
    pub handle: Arc<str>,
    pub cwd: PathBuf,
    pub chat_tools: ChatToolsEndpoint,
    pub events: EventSink,
    /// Where native permission requests go; the adapter answers the agent with
    /// whatever core decides (see `permission`).
    pub permissions: PermissionGate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentErrorKind {
    /// The agent needs a sign-in. Retrying will not help.
    Auth,
    /// Could not start the process.
    Spawn,
    /// The agent answered, but not with something we can use.
    Protocol,
    /// The connection broke.
    Transport,
}

#[derive(Clone, Debug)]
pub struct AgentError {
    pub kind: AgentErrorKind,
    pub message: Arc<str>,
}

impl AgentError {
    pub fn new(kind: AgentErrorKind, message: impl Into<Arc<str>>) -> Self {
        AgentError {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for AgentError {}

/// How a turn ended, normalised across protocols.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Refused,
    MaxTokens,
    Other(Arc<str>),
}

/// Normalised events out of a session. No token deltas: text is collected by
/// the adapter and handed over once, at turn end.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// The adapter's delivery-accepted point for `turn` (see the module docs).
    Accepted { turn: BatchId },
    /// Coarse activity ("Running tests"). Presence only, never the chat.
    Activity { label: Arc<str> },
    /// The turn finished. `final_text` is the agent's last assistant text, a
    /// fallback-reply candidate; it is never posted on its own.
    TurnEnded {
        turn: BatchId,
        outcome: TurnOutcome,
        final_text: Arc<str>,
    },
    /// The turn could not finish.
    TurnFailed {
        turn: BatchId,
        accepted: bool,
        error: AgentError,
    },
    /// The agent process or connection is gone. Sent after `TurnFailed` for a
    /// turn that was in flight.
    Exited { error: Option<Arc<str>> },
}

pub trait AgentSession: Send + Sync {
    fn info(&self) -> &SessionInfo;
    /// Hands one prompt to the agent as a new turn (see the delivery contract).
    fn deliver(&self, turn: BatchId, prompt: Arc<str>) -> BoxFuture<'_, Result<(), AgentError>>;
    /// Asks the agent to stop the running turn.
    fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>>;
    /// Closes politely and ends the process.
    fn close(&self) -> BoxFuture<'_, ()>;
}

pub trait AgentAdapter: Send + Sync {
    /// Human-readable, for logs and tooltips ("acp: npx claude-agent-acp").
    fn describe(&self) -> String;
    /// Cheap and side-effect free (see [`Probe`]).
    fn probe(&self) -> BoxFuture<'_, Probe>;
    /// Starts a session: negotiates the protocol, injects chat tools.
    fn open(&self, req: OpenSession) -> BoxFuture<'_, Result<Arc<dyn AgentSession>, AgentError>>;
    /// Asks the harness which models it offers, without joining any chat:
    /// for ACP a throwaway `session/new` (no chat tools) that is closed at
    /// once, for Codex `model/list`. Spawns a process, so the host only
    /// calls it when the user opens the model picker, and caches the answer.
    /// `None` when this adapter cannot list models.
    fn list_models(
        &self,
        cwd: PathBuf,
    ) -> BoxFuture<'_, Option<Result<Vec<ModelInfo>, AgentError>>> {
        let _ = cwd;
        Box::pin(async { None })
    }
}

/// Where a member's model choice ended up, for `SessionInfo`.
pub(crate) fn model_outcome(
    wanted: Option<&str>,
    offered: &[ModelInfo],
    applied: Result<(), String>,
) -> Option<Arc<str>> {
    let wanted = wanted?;
    if !offered.is_empty() && !offered.iter().any(|m| m.id.as_deref() == Some(wanted)) {
        return Some(Arc::from(format!(
            "this harness doesn't offer {wanted} now; running its default"
        )));
    }
    match applied {
        Ok(()) => None,
        Err(e) => Some(Arc::from(format!(
            "couldn't switch to {wanted} ({e}); running the harness default"
        ))),
    }
}
