//! Permissions by where the agent runs (STRM-401). The rules are written up in
//! `docs/agents.md` ("Permissions").
//!
//! - **On the host** (the user's computer) a permission request goes to the
//!   humans and never blocks the agent. The agent hears "request sent" with a
//!   request id at once and decides whether to wait or do something else. A
//!   human's Approve or Deny arrives later, as a message that wakes the agent
//!   (its next turn). Nothing is ever approved automatically and nothing times
//!   out into an approval.
//! - **In the agent's own VM** ([`RunsIn::Vm`]) everything is approved: a broken
//!   VM is replaced, not protected.
//!
//! A protocol whose permission call blocks (ACP `session/request_permission`)
//! is answered at once: with the agent's own allow-once option when the
//! request is approved (VM, or a one-shot grant a human already gave for that
//! exact request), else with its own decline ("not now") option and a `_meta`
//! note that a request was sent. An approval becomes a one-shot grant: when the
//! agent retries the exact same tool call in that channel, it goes through
//! once. "The same" is the request's key ([`request_key`]): the tool kind plus
//! a SHA-256 of what the call would do, without its ids (STRM-572), so an
//! approved `Edit files` on `a.rs` never lets an `Edit files` on `b.rs`
//! through, and a retry with a new tool call id still matches. Agents can
//! also ask ahead, and poll, through the chat tools `chat_request_permission`
//! and `chat_permission_status`.
//!
//! Peer model: a declined or denied request is an answer, not a failure. It
//! never turns an agent red or fails its turn.
//!
//! **Reads and chat tools are always allowed.** A native request for a
//! read-only tool call (ACP tool kind `read` or `search`) is answered with the
//! agent's allow-once option straight away, wherever the agent runs: agents
//! get full access for reading files. It is recorded for the permission log
//! only (`read_only`, state `auto_approved`, the newest [`READS_KEPT`] per
//! channel), never as a request anyone answers, and the agent's own
//! `chat_permission_status` leaves it out. A native request for one of this
//! app's named chat MCP tools is also auto-approved and logged by its tool
//! name. Everything else (running commands, editing, deleting, moving,
//! fetching, an unknown or missing kind) goes the way above: approved in a
//! VM, else declined at once with a message saying why and that a human was
//! asked. Codex has no read requests to answer; its sessions start in a
//! read-only sandbox instead (see `codex.rs`).

use std::{
    fmt::{self, Write as _},
    sync::Arc,
};

use futures::future::BoxFuture;
use rmcp::model::Tool;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    engine::{CoreEvent, Effect, Engine, PostError},
    mcp::{Scope, ToolError},
    store,
    types::*,
};

// ---------- types ----------

/// Where an agent runs, which decides how its permission requests are handled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunsIn {
    /// The user's computer: requests go to the humans, non-blocking.
    #[default]
    Host,
    /// The agent's own VM: every request is approved.
    Vm,
}

/// A permission request's id. Stable, never reused (`AUTOINCREMENT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PermissionId(pub i64);

impl fmt::Display for PermissionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Where a request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionSource {
    /// The agent's protocol asked (ACP `session/request_permission`).
    Native,
    /// The agent asked ahead with `chat_request_permission`.
    ChatTool,
}

impl PermissionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionSource::Native => "native",
            PermissionSource::ChatTool => "chat_tool",
        }
    }

    pub fn parse(s: &str) -> PermissionSource {
        match s {
            "chat_tool" => PermissionSource::ChatTool,
            _ => PermissionSource::Native,
        }
    }
}

/// Where a request stands. `Pending` only ever moves to `Approved` or
/// `Denied`, by a human; `AutoApproved` is written as such (VM agents).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Pending,
    Approved,
    Denied,
    AutoApproved,
}

impl PermissionState {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionState::Pending => "pending",
            PermissionState::Approved => "approved",
            PermissionState::Denied => "denied",
            PermissionState::AutoApproved => "auto_approved",
        }
    }

    pub fn parse(s: &str) -> PermissionState {
        match s {
            "approved" => PermissionState::Approved,
            "denied" => PermissionState::Denied,
            "auto_approved" => PermissionState::AutoApproved,
            _ => PermissionState::Pending,
        }
    }
}

/// One permission request, as stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PermissionRequest {
    pub id: PermissionId,
    pub agent: MemberId,
    pub channel: ChannelId,
    /// The turn it was asked in, if one was running.
    pub batch: Option<BatchId>,
    pub source: PermissionSource,
    /// What it wants to do: the tool call's title ("rm -rf build/").
    pub title: Arc<str>,
    /// The tool's raw input, when the protocol gave one (display only).
    pub detail: Option<Arc<str>>,
    /// Why, in the agent's words (chat tool only).
    pub reason: Option<Arc<str>>,
    pub state: PermissionState,
    /// The humans it was routed to: whoever woke the agent (else every
    /// human in the channel). Any human member can answer.
    pub asked_of: Vec<MemberId>,
    pub created_at: Millis,
    pub answered_by: Option<MemberId>,
    pub answered_at: Option<Millis>,
    pub answer_note: Option<Arc<str>>,
    /// When an approval was used up by the retried tool call (one-shot grant).
    pub used_at: Option<Millis>,
    /// A read-only tool call, allowed at once and kept for the log only.
    pub read_only: bool,
    /// What exactly was asked for ([`request_key`]): grants and dedup match
    /// on it. None for logged reads, auto-allowed chat tools, and requests
    /// stored before migration 13.
    pub request_key: Option<Arc<str>>,
    /// Who or what decided this request's outcome (STRM-542). None for
    /// requests stored before migration 16.
    pub decided_by: Option<DecidedBy>,
}

/// How a request ended, as the permission log shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogOutcome {
    /// Allowed without a human: a read, or anything from an agent in its own VM.
    AutoAllowed,
    /// The agent was told "not now" (pending, never answered) or a human denied it.
    Declined,
    /// A human approved it (a one-shot grant).
    Approved,
}

impl LogOutcome {
    pub fn label(self) -> &'static str {
        match self {
            LogOutcome::AutoAllowed => "Auto-allowed",
            LogOutcome::Declined => "Declined",
            LogOutcome::Approved => "Approved",
        }
    }
}

impl PermissionRequest {
    /// The outcome the permission log shows. A pending request counts as
    /// declined: the agent's tool call was declined at once, whatever a human
    /// may say later.
    pub fn outcome(&self) -> LogOutcome {
        match self.state {
            PermissionState::AutoApproved => LogOutcome::AutoAllowed,
            PermissionState::Approved => LogOutcome::Approved,
            PermissionState::Pending | PermissionState::Denied => LogOutcome::Declined,
        }
    }
}

/// A native permission request, as the adapter hands it to core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PermissionAsk {
    pub turn: Option<BatchId>,
    pub title: Arc<str>,
    /// ACP's machine-readable tool name, when supplied.
    pub tool_name: Option<Arc<str>>,
    pub detail: Option<Arc<str>>,
    /// The tool call only reads (ACP tool kind `read` or `search`). Reads are
    /// always allowed; a missing kind is not a read.
    pub read_only: bool,
    /// The request's key ([`request_key`]), which a grant must match. None
    /// falls back to a key on the title alone.
    pub key: Option<Arc<str>>,
}

/// How the adapter must answer a native permission request, right away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionVerdict {
    /// Pick the agent's own allow-once option (VM agent, or a human's grant).
    Allow { request: PermissionId },
    /// Pick the agent's own allow-once option for a read-only tool call.
    /// Reads are always allowed and not recorded.
    AllowRead,
    /// Pick the agent's own decline ("not now") option; the request went to
    /// the humans and the answer arrives later.
    Sent { request: PermissionId },
    /// The request is queued for deferred execution (accept-edits mode).
    DeferredQueue { request: PermissionId },
    /// The request is held open for blocking queue (when caps.hold_request).
    Hold { request: PermissionId },
    /// No core to ask (replay tests): decline, nothing recorded.
    Decline,
}

/// What kind of operation a permission request is for (STRM-542).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    /// A read-only operation (file read, search).
    Read,
    /// An edit operation (file write, delete, move).
    Edit,
    /// A command execution.
    Execute,
    /// A fetch operation (HTTP, etc.).
    Fetch,
    /// Some other kind of operation.
    Other,
}

/// What a mode does with one incoming request (STRM-542).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeOutcome {
    /// Allow immediately.
    Allow,
    /// Ask a human.
    AskHuman,
    /// Queue for later execution.
    Queue,
}

/// How an adapter asks core about a native permission request. Cheap to clone.
#[derive(Clone)]
pub struct PermissionGate(
    Arc<dyn Fn(PermissionAsk) -> BoxFuture<'static, PermissionVerdict> + Send + Sync>,
);

impl PermissionGate {
    pub fn new(
        f: impl Fn(PermissionAsk) -> BoxFuture<'static, PermissionVerdict> + Send + Sync + 'static,
    ) -> Self {
        PermissionGate(Arc::new(f))
    }

    /// A gate that declines everything and records nothing (replay tests).
    pub fn decline_all() -> Self {
        PermissionGate::new(|_| Box::pin(async { PermissionVerdict::Decline }))
    }

    pub fn ask(&self, ask: PermissionAsk) -> BoxFuture<'static, PermissionVerdict> {
        (self.0)(ask)
    }
}

impl fmt::Debug for PermissionGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PermissionGate")
    }
}

/// What the agent says it wants to change, when the protocol gives us
/// enough to show a human (STRM-542). `None` means "show the title/detail only" —
/// we never fabricate a diff from the command line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum ProposedChange {
    /// Old/new text for one file (ACP diff content).
    Text {
        path: Arc<str>,
        old: Arc<str>,
        new: Arc<str>,
    },
    /// A unified diff blob, when that's all the agent sent.
    Unified {
        path: Arc<str>,
        patch: Arc<str>,
    },
}

/// What one live session's protocol can actually do for us with permission
/// requests (STRM-542). Sampled from the handshake/negotiation.
#[derive(Clone, Copy, Debug, Default)]
pub struct ApprovalCaps {
    /// The agent sends a structured diff we can show a human.
    pub proposed_diffs: bool,
    /// The permission call can be held open past the initial ask
    /// (true queue, blocking). Vanilla ACP: false.
    pub hold_request: bool,
    /// The harness can execute an approved command itself and report
    /// output back through a channel we can read (rare; most agents
    /// only retry through their own tool call).
    pub deferred_execute: bool,
    /// The agent's "accept edits"-style mode can be set through the
    /// protocol rather than only mimicked by us.
    pub mode_switch: bool,
}

/// One thing an agent wants to do that a human (or a mode) hasn't settled.
/// This is the row the sidebar renders and the app's Approve/Deny acts on.
/// Wraps/extends `permission_requests`; the store table is the same,
/// the queue is the *view* plus the delivery bookkeeping (STRM-542).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueuedRequest {
    pub id: PermissionId,
    pub agent: MemberId,
    pub channel: ChannelId,
    /// What it wants, for display: title, detail, proposed diff if the
    /// agent sent one (see `ProposedChange`).
    pub ask: PermissionAsk,
    /// Where it sits in the queue: position is display order, stable
    /// across re-renders. Approving out of order is allowed — each
    /// request is independent.
    pub state: QueueState,
    /// For `accept-edits` commands: the captured command line and the
    /// working directory it would run in. Empty for plain asks.
    pub command: Option<Arc<str>>,
    /// What the agent says it wants to change (when available).
    pub proposed_change: Option<ProposedChange>,
}

/// Where a queued request sits in its lifecycle (STRM-542).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueState {
    /// Waiting for a human (or for its mode rule to apply).
    Pending,
    /// Approved; the command is running (accept-edits) or the grant is
    /// armed for the agent's retry (ask-all).
    Approved,
    /// Denied by a human, with the reason that will be steered back.
    Denied,
    /// Ran to completion; `result` is what gets delivered to the agent.
    Done,
    /// Approved but execution failed (command exited non-zero, agent
    /// unreachable). The agent hears the failure, not silence.
    Failed,
}

/// The `_meta` an adapter attaches to a native decline after a request was
/// sent, so the agent (or a person reading the transcript) knows it was "not
/// now", not "no".
pub fn sent_meta(request: PermissionId) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "chaosControl".into(),
        json!({
            "status": "request_sent",
            "requestId": request.0,
            "message": sent_message(request),
        }),
    );
    m
}

fn sent_message(request: PermissionId) -> String {
    format!(
        "Declined for now: in Chaos Control you can read files freely, but running commands \
and editing, moving or deleting files need a human's approval. This one was sent to the humans \
as permission request #{request}. Don't wait for it: carry on with what you can do by reading, \
or say in the chat what you would change. If a human approves, the answer arrives as a message \
in a later turn (chat_permission_status shows it); then retry the same call and it goes through once."
    )
}

/// Whether an ACP tool kind (as its wire string: `read`, `search`, ...) only
/// reads. Reads are always allowed; everything else needs approval.
pub fn is_read_only_kind(kind: &str) -> bool {
    matches!(kind, "read" | "search")
}

/// What a mode does with one incoming request, before any human sees it.
/// Pure function of (mode, request kind) — no I/O, fully testable (STRM-542).
pub fn mode_verdict(mode: ApprovalMode, kind: RequestKind) -> ModeOutcome {
    match mode {
        // Reads never reach this function: is_read_only_kind short-circuits
        // upstream, as today.
        ApprovalMode::AskAll => ModeOutcome::AskHuman,
        ApprovalMode::AcceptEdits => match kind {
            RequestKind::Edit | RequestKind::Read => ModeOutcome::Allow,
            RequestKind::Execute | RequestKind::Fetch | RequestKind::Other => ModeOutcome::Queue,
        },
        ApprovalMode::Yolo => ModeOutcome::Allow,
    }
}

/// The decision the gate makes, given caps. Fail closed: a capability
/// we don't have means we decline-and-ask, never allow-and-hope (STRM-542).
pub fn decide(
    caps: ApprovalCaps,
    mode: ApprovalMode,
    kind: RequestKind,
) -> PermissionVerdict {

    match mode_verdict(mode, kind) {
        ModeOutcome::Allow => PermissionVerdict::AllowRead,
        ModeOutcome::AskHuman => {
            if caps.hold_request {
                // True blocking queue: keep the ACP call open, answer
                // when the human resolves. Agent waits — acceptable only
                // because the agent (or mode) opted into hold semantics.
                PermissionVerdict::Hold {
                    request: PermissionId(0), // placeholder, set by caller
                }
            } else {
                // Universal path: decline now with "request sent #N",
                // grant on retry. Today's behavior, kept.
                PermissionVerdict::Sent {
                    request: PermissionId(0), // placeholder, set by caller
                }
            }
        }
        ModeOutcome::Queue => {
            if caps.deferred_execute {
                PermissionVerdict::DeferredQueue {
                    request: PermissionId(0), // placeholder, set by caller
                }
            } else {
                // Can't run it for them: queue the *ask*, agent retries
                // on approval like ask-all, output comes back as a wake.
                PermissionVerdict::Sent {
                    request: PermissionId(0), // placeholder, set by caller
                }
            }
        }
    }
}

// ---------- request keys ----------

/// The fields of an ACP tool call that differ between two calls of the same
/// thing: its ids, its progress and what it produced. Left out of the key.
pub const ACP_VOLATILE: &[&str] = &["toolCallId", "terminalId", "status", "rawOutput", "_meta"];

/// What a request asks for, as a key a grant must match (STRM-572): the tool
/// kind, a `:`, then the SHA-256 (hex) of `payload` as canonical JSON (object
/// keys sorted at every depth, no whitespace). Strip the volatile fields
/// first ([`without`]); a request id must never be part of the payload, so an
/// approved call retried with a new id still matches.
pub fn request_key(kind: &str, payload: &Value) -> Arc<str> {
    let mut canon = String::new();
    canonical(payload, &mut canon);
    let digest = Sha256::digest(canon.as_bytes());
    let kind = kind.trim();
    let mut key = String::with_capacity(kind.len() + 65);
    key.push_str(if kind.is_empty() { "none" } else { kind });
    key.push(':');
    for b in digest {
        let _ = write!(key, "{b:02x}");
    }
    Arc::from(key)
}

fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (ix, k) in keys.into_iter().enumerate() {
                if ix > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical(&m[k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (ix, item) in a.iter().enumerate() {
                if ix > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// `payload` without the `volatile` fields, at any depth, except inside a
/// `rawInput`: that is the tool's own input, hashed exactly as given.
pub fn without(payload: &Value, volatile: &[&str]) -> Value {
    match payload {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| !volatile.contains(&k.as_str()))
                .map(|(k, v)| {
                    let v = if k == "rawInput" {
                        v.clone()
                    } else {
                        without(v, volatile)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|v| without(v, volatile)).collect()),
        other => other.clone(),
    }
}

pub(crate) fn permission_title(title: Option<&str>, name: Option<&str>) -> Arc<str> {
    let title = title.map(str::trim).filter(|s| !s.is_empty());
    let name = name.map(str::trim).filter(|s| !s.is_empty());
    Arc::from(match title {
        Some("a tool") | None => name.unwrap_or("a tool"),
        Some(title) => title,
    })
}

// ---------- picking the agent's own option ----------

/// The kinds of option an agent offers, across protocol versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Other,
}

/// The agent's allow option: allow-once, else allow-always. Allow-always can
/// write a lasting rule into the agent's own settings (claude-agent-acp writes
/// local settings), so it is only picked when nothing narrower is offered.
pub fn pick_allow(options: &[(OptionKind, &str)]) -> Option<usize> {
    options
        .iter()
        .position(|(k, _)| *k == OptionKind::AllowOnce)
        .or_else(|| {
            options
                .iter()
                .position(|(k, _)| *k == OptionKind::AllowAlways)
        })
}

/// The agent's "not now" option: a reject-once that lets the turn continue,
/// else any reject-once, else reject-always. Codex offers two reject-once
/// options ("No, continue without running it" and "No, and tell Codex what to
/// do differently", which aborts the turn); the one that says "continue" wins.
pub fn pick_decline(options: &[(OptionKind, &str)]) -> Option<usize> {
    let once = |pred: &dyn Fn(&str) -> bool| {
        options
            .iter()
            .position(|(k, name)| *k == OptionKind::RejectOnce && pred(name))
    };
    once(&|name| name.to_lowercase().contains("continue"))
        .or_else(|| once(&|_| true))
        .or_else(|| {
            options
                .iter()
                .position(|(k, _)| *k == OptionKind::RejectAlways)
        })
}

// ---------- store ----------

const COLS: &str = "id, agent_id, channel_id, batch_id, source, title, detail, reason, state, \
asked_of, created_at, answered_by, answered_at, answer_note, used_at, read_only, request_key, decided_by";

fn row(r: &Row<'_>) -> rusqlite::Result<PermissionRequest> {
    let asked: String = r.get(9)?;
    Ok(PermissionRequest {
        id: PermissionId(r.get(0)?),
        agent: MemberId(r.get(1)?),
        channel: ChannelId(r.get(2)?),
        batch: r.get::<_, Option<i64>>(3)?.map(BatchId),
        source: PermissionSource::parse(&r.get::<_, String>(4)?),
        title: Arc::from(r.get::<_, String>(5)?),
        detail: r.get::<_, Option<String>>(6)?.map(Arc::from),
        reason: r.get::<_, Option<String>>(7)?.map(Arc::from),
        state: PermissionState::parse(&r.get::<_, String>(8)?),
        asked_of: asked
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .map(MemberId)
            .collect(),
        created_at: r.get::<_, i64>(10)? as Millis,
        answered_by: r.get::<_, Option<i64>>(11)?.map(MemberId),
        answered_at: r.get::<_, Option<i64>>(12)?.map(|t| t as Millis),
        answer_note: r.get::<_, Option<String>>(13)?.map(Arc::from),
        used_at: r.get::<_, Option<i64>>(14)?.map(|t| t as Millis),
        read_only: r.get::<_, i64>(15)? != 0,
        request_key: r.get::<_, Option<String>>(16)?.map(Arc::from),
        decided_by: r
            .get::<_, Option<String>>(17)?
            .and_then(|s| DecidedBy::parse(&s)),
    })
}

/// A request about to be stored.
pub struct NewRequest<'a> {
    pub agent: MemberId,
    pub channel: ChannelId,
    pub batch: Option<BatchId>,
    pub source: PermissionSource,
    pub title: &'a str,
    pub detail: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub state: PermissionState,
    pub asked_of: &'a [MemberId],
    pub now: Millis,
    pub read_only: bool,
    /// See [`PermissionRequest::request_key`].
    pub request_key: Option<&'a str>,
    /// See [`PermissionRequest::decided_by`].
    pub decided_by: Option<DecidedBy>,
}

pub fn insert(c: &Connection, r: &NewRequest<'_>) -> rusqlite::Result<PermissionRequest> {
    let asked = r
        .asked_of
        .iter()
        .map(|m| m.0.to_string())
        .collect::<Vec<_>>()
        .join(",");
    c.execute(
        "INSERT INTO permission_requests
            (agent_id, channel_id, batch_id, source, title, detail, reason, state, asked_of,
             created_at, read_only, request_key, decided_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            r.agent.0,
            r.channel.0,
            r.batch.map(|b| b.0),
            r.source.as_str(),
            r.title,
            r.detail,
            r.reason,
            r.state.as_str(),
            asked,
            r.now as i64,
            r.read_only as i64,
            r.request_key,
            r.decided_by.as_ref().map(|d| d.as_str())
        ],
    )?;
    let id = PermissionId(c.last_insert_rowid());
    get(c, id).map(|r| r.expect("just inserted"))
}

pub fn get(c: &Connection, id: PermissionId) -> rusqlite::Result<Option<PermissionRequest>> {
    c.query_row(
        &format!("SELECT {COLS} FROM permission_requests WHERE id = ?1"),
        [id.0],
        row,
    )
    .optional()
}

/// An approved request for exactly this (agent, channel, request key) that
/// has not been used yet: the one-shot grant a retried tool call consumes.
pub fn unused_grant(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    key: &str,
) -> rusqlite::Result<Option<PermissionId>> {
    c.query_row(
        "SELECT id FROM permission_requests
         WHERE agent_id = ?1 AND channel_id = ?2 AND request_key = ?3
           AND state = 'approved' AND used_at IS NULL
         ORDER BY id LIMIT 1",
        params![agent.0, channel.0, key],
        |r| r.get(0).map(PermissionId),
    )
    .optional()
}

/// A still-pending request with the same key, so asking twice for the same
/// thing never sends the humans two requests.
pub fn pending_for(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    key: &str,
) -> rusqlite::Result<Option<PermissionRequest>> {
    c.query_row(
        &format!(
            "SELECT {COLS} FROM permission_requests
             WHERE agent_id = ?1 AND channel_id = ?2 AND request_key = ?3 AND state = 'pending'
             ORDER BY id LIMIT 1"
        ),
        params![agent.0, channel.0, key],
        row,
    )
    .optional()
}

/// The agent's requests in a channel, newest first. Logged reads are left
/// out: nobody asked anyone for them.
pub fn of_agent(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    limit: usize,
) -> rusqlite::Result<Vec<PermissionRequest>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {COLS} FROM permission_requests
         WHERE agent_id = ?1 AND channel_id = ?2 AND read_only = 0
         ORDER BY id DESC LIMIT ?3"
    ))?;
    stmt.query_map(params![agent.0, channel.0, limit as i64], row)?
        .collect()
}

/// Every pending request in a channel, oldest first.
pub fn pending_in(c: &Connection, channel: ChannelId) -> rusqlite::Result<Vec<PermissionRequest>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {COLS} FROM permission_requests
         WHERE channel_id = ?1 AND state = 'pending' ORDER BY id"
    ))?;
    stmt.query_map([channel.0], row)?.collect()
}

/// How many logged reads a channel keeps. Reads are frequent and only
/// matter for the log, so older ones are pruned as new ones arrive; every
/// other request is kept.
pub const READS_KEPT: usize = 500;

/// The permission log for a channel: every request, reads included, newest
/// first, at most `limit`.
pub fn log_in(
    c: &Connection,
    channel: ChannelId,
    limit: usize,
) -> rusqlite::Result<Vec<PermissionRequest>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {COLS} FROM permission_requests
         WHERE channel_id = ?1 ORDER BY id DESC LIMIT ?2"
    ))?;
    stmt.query_map(params![channel.0, limit as i64], row)?
        .collect()
}

/// Drops a channel's logged reads past the newest `keep`. Returns how many
/// went.
pub fn prune_reads(c: &Connection, channel: ChannelId, keep: usize) -> rusqlite::Result<usize> {
    c.execute(
        "DELETE FROM permission_requests
         WHERE channel_id = ?1 AND read_only = 1 AND id <= (
             SELECT id FROM permission_requests
             WHERE channel_id = ?1 AND read_only = 1
             ORDER BY id DESC LIMIT 1 OFFSET ?2)",
        params![channel.0, keep as i64],
    )
}

/// Records a read-only tool call that was allowed at once, for the log, and
/// prunes the channel's older reads.
pub fn insert_read(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    batch: Option<BatchId>,
    title: &str,
    detail: Option<&str>,
    now: Millis,
) -> rusqlite::Result<PermissionRequest> {
    let r = insert(
        c,
        &NewRequest {
            agent,
            channel,
            batch,
            source: PermissionSource::Native,
            title,
            detail,
            reason: None,
            state: PermissionState::AutoApproved,
            asked_of: &[],
            now,
            read_only: true,
            request_key: None,
            decided_by: Some(DecidedBy::AutoVm),
        },
    )?;
    prune_reads(c, channel, READS_KEPT)?;
    Ok(r)
}

/// Records a human's answer. Only a pending request can be answered; returns
/// whether this call answered it.
pub fn answer(
    c: &Connection,
    id: PermissionId,
    state: PermissionState,
    by: MemberId,
    note: Option<&str>,
    now: Millis,
) -> rusqlite::Result<bool> {
    let decided_by = DecidedBy::Human(by);
    let n = c.execute(
        "UPDATE permission_requests
         SET state = ?2, answered_by = ?3, answered_at = ?4, answer_note = ?5, decided_by = ?6
         WHERE id = ?1 AND state = 'pending'",
        params![id.0, state.as_str(), by.0, now as i64, note, decided_by.as_str()],
    )?;
    Ok(n == 1)
}

pub fn mark_used(c: &Connection, id: PermissionId, now: Millis) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE permission_requests SET used_at = ?2 WHERE id = ?1 AND used_at IS NULL",
        params![id.0, now as i64],
    )?;
    Ok(())
}

// ---------- engine ----------

/// Why a human's answer was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnswerError {
    UnknownRequest,
    /// Already approved or denied (by someone else, or twice).
    AlreadyAnswered(PermissionState),
    /// Only humans in the request's channel can answer.
    NotAllowed,
    Store(String),
}

impl fmt::Display for AnswerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnswerError::UnknownRequest => f.write_str("no such permission request"),
            AnswerError::AlreadyAnswered(s) => write!(f, "already {}", s.as_str()),
            AnswerError::NotAllowed => {
                f.write_str("only a human in that channel can answer a permission request")
            }
            AnswerError::Store(e) => write!(f, "store error: {e}"),
        }
    }
}

impl From<rusqlite::Error> for AnswerError {
    fn from(e: rusqlite::Error) -> Self {
        AnswerError::Store(e.to_string())
    }
}

impl From<PostError> for AnswerError {
    fn from(e: PostError) -> Self {
        AnswerError::Store(e.to_string())
    }
}

fn handle_of(c: &Connection, id: MemberId) -> Arc<str> {
    store::member(c, id)
        .ok()
        .flatten()
        .map(|m| m.handle)
        .unwrap_or_else(|| Arc::from("someone"))
}

fn short(title: &str) -> String {
    let one_line = title.lines().next().unwrap_or_default();
    if one_line.chars().count() > 80 || title.contains('\n') {
        one_line.chars().take(80).collect::<String>() + "…"
    } else {
        one_line.to_string()
    }
}

/// The presence line while a request is pending (never "blocked": the agent
/// may well be doing something else).
pub fn pending_presence(c: &Connection, agent: MemberId, channel: ChannelId) -> Option<Arc<str>> {
    let pending: Vec<PermissionRequest> = of_agent(c, agent, channel, 20)
        .ok()?
        .into_iter()
        .filter(|r| r.state == PermissionState::Pending)
        .collect();
    let newest = pending.first()?;
    let more = match pending.len() {
        1 => String::new(),
        n => format!(" (+{} more)", n - 1),
    };
    Some(Arc::from(format!(
        "request pending: {} (#{}){more}",
        short(&newest.title),
        newest.id
    )))
}

impl Engine {
    /// Marks where an agent runs. Agents default to the host.
    pub fn set_runs_in(&mut self, agent: MemberId, runs_in: RunsIn) {
        self.runs_in.insert(agent, runs_in);
    }

    pub fn runs_in(&self, agent: MemberId) -> RunsIn {
        self.runs_in.get(&agent).copied().unwrap_or_default()
    }

    /// The humans a request from `agent` goes to: whoever woke the turn it was
    /// asked in, else every human in the channel.
    fn route(&self, channel: ChannelId, batch: Option<BatchId>) -> Vec<MemberId> {
        let c = self.conn();
        let mut out = Vec::new();
        if let Some(b) = batch {
            for t in store::batch_triggers(c, b).unwrap_or_default() {
                if let Ok(Some(m)) = store::message(c, t.id.message)
                    && let Some(a) = m.author
                    && let Ok(Some(who)) = store::member(c, a)
                    && who.kind == MemberKind::Human
                    && !out.contains(&a)
                {
                    out.push(a);
                }
            }
        }
        if out.is_empty() {
            out = store::members_of(c, channel)
                .unwrap_or_default()
                .into_iter()
                .filter(|m| m.kind == MemberKind::Human)
                .map(|m| m.id)
                .collect();
        }
        out
    }

    /// A native permission request from `agent`'s session in `channel`.
    /// Decides how the adapter answers right now (see the module docs).
    pub fn native_permission(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        ask: PermissionAsk,
    ) -> PermissionVerdict {
        if let Some(name) = ask
            .tool_name
            .as_deref()
            .filter(|name| crate::mcp::TOOL_NAMES.contains(name))
        {
            return match self.auto_approve_chat_tool(
                now,
                agent,
                channel,
                ask.turn,
                name,
                ask.detail.as_deref(),
            ) {
                Ok(r) => PermissionVerdict::Allow { request: r.id },
                Err(e) => {
                    self.rt_note(agent, channel, format!("internal error: {e}"));
                    PermissionVerdict::Decline
                }
            };
        }
        if ask.read_only {
            self.rt_note(agent, channel, format!("reading: {}", short(&ask.title)));
            self.log_read(now, agent, channel, &ask);
            return PermissionVerdict::AllowRead;
        }
        match self.ask_permission(
            now,
            agent,
            channel,
            ask.turn,
            PermissionSource::Native,
            &ask.title,
            ask.detail.as_deref(),
            None,
            ask.key.as_deref(),
        ) {
            Ok(r) => match r.state {
                PermissionState::Approved | PermissionState::AutoApproved => {
                    PermissionVerdict::Allow { request: r.id }
                }
                PermissionState::Pending | PermissionState::Denied => {
                    PermissionVerdict::Sent { request: r.id }
                }
            },
            Err(e) => {
                self.rt_note(agent, channel, format!("internal error: {e}"));
                PermissionVerdict::Decline
            }
        }
    }

    fn auto_approve_chat_tool(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        turn: Option<BatchId>,
        name: &str,
        detail: Option<&str>,
    ) -> Result<PermissionRequest, PostError> {
        let turn = match turn {
            Some(t) => Some(t),
            None => store::in_flight_batch(self.conn(), agent, channel)?.map(|b| b.id),
        };
        let r = self.store.tx(|c| {
            insert(
                c,
                &NewRequest {
                    agent,
                    channel,
                    batch: turn,
                    source: PermissionSource::Native,
                    title: name,
                    detail,
                    reason: None,
                    state: PermissionState::AutoApproved,
                    asked_of: &[],
                    now,
                    read_only: false,
                    request_key: None,
                    decided_by: Some(DecidedBy::AutoVm),
                },
            )
        })?;
        self.rt_note(
            agent,
            channel,
            format!("using {} (chat tool, auto-allowed)", short(name)),
        );
        self.effects
            .push(Effect::Event(CoreEvent::Permission(r.clone())));
        Ok(r)
    }

    /// Records a request and works out its state: auto-approved in a VM, the
    /// grant a human already gave (used up here), the same request already
    /// pending, or a new pending request for the humans. "The same" means the
    /// same `key` ([`request_key`]); without one, a key on the title alone.
    #[allow(clippy::too_many_arguments)]
    pub fn ask_permission(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        turn: Option<BatchId>,
        source: PermissionSource,
        title: &str,
        detail: Option<&str>,
        reason: Option<&str>,
        key: Option<&str>,
    ) -> Result<PermissionRequest, PostError> {
        let title = title.trim();
        let title = if title.is_empty() { "a tool" } else { title };
        let key: Arc<str> = match key {
            Some(k) => Arc::from(k),
            None => request_key("title", &json!(title)),
        };
        let turn = match turn {
            Some(t) => Some(t),
            None => store::in_flight_batch(self.conn(), agent, channel)?.map(|b| b.id),
        };
        let handle = handle_of(self.conn(), agent);
        if self.runs_in(agent) == RunsIn::Vm {
            let r = self.store.tx(|c| {
                insert(
                    c,
                    &NewRequest {
                        agent,
                        channel,
                        batch: turn,
                        source,
                        title,
                        detail,
                        reason,
                        state: PermissionState::AutoApproved,
                        asked_of: &[],
                        now,
                        read_only: false,
                        request_key: Some(&*key),
                        decided_by: Some(DecidedBy::AutoVm),
                    },
                )
            })?;
            self.rt_note(
                agent,
                channel,
                format!("ran {} (own VM, auto-approved)", short(title)),
            );
            self.effects
                .push(Effect::Event(CoreEvent::Permission(r.clone())));
            return Ok(r);
        }
        if let Some(grant) = unused_grant(self.conn(), agent, channel, &key)? {
            if source == PermissionSource::ChatTool {
                // Asking ahead for something already approved: still approved,
                // and still unused until the tool call itself runs.
                return Ok(get(self.conn(), grant)?.expect("grant exists"));
            }
            self.store.tx(|c| mark_used(c, grant, now))?;
            let r = get(self.conn(), grant)?.expect("grant exists");
            self.rt_note(
                agent,
                channel,
                format!("running {} (approved, #{grant})", short(title)),
            );
            self.effects
                .push(Effect::Event(CoreEvent::Permission(r.clone())));
            return Ok(r);
        }
        if let Some(r) = pending_for(self.conn(), agent, channel, &key)? {
            return Ok(r);
        }
        let asked_of = self.route(channel, turn);
        let r = self.store.tx(|c| {
            insert(
                c,
                &NewRequest {
                    agent,
                    channel,
                    batch: turn,
                    source,
                    title,
                    detail,
                    reason,
                    state: PermissionState::Pending,
                    asked_of: &asked_of,
                    now,
                    read_only: false,
                    request_key: Some(&*key),
                    decided_by: None,
                },
            )
        })?;
        if self.policy().note_permission_requests {
            let who: Vec<String> = asked_of
                .iter()
                .map(|m| format!("@{}", handle_of(self.conn(), *m)))
                .collect();
            let why = reason
                .map(|w| format!(" ({})", w.trim()))
                .unwrap_or_default();
            let to = if who.is_empty() {
                String::new()
            } else {
                format!(", sent to {}", who.join(", "))
            };
            self.system_note(
                now,
                channel,
                &format!(
                    "@{handle} asked to run {}{why}: permission request #{}{to}. Any human here can approve or deny it; @{handle} carries on meanwhile.",
                    short(title),
                    r.id
                ),
            )?;
        }
        self.effects
            .push(Effect::Event(CoreEvent::Permission(r.clone())));
        self.publish_status(agent, channel);
        Ok(r)
    }

    /// A human approves or denies a pending request. The answer is posted as
    /// a system message and delivered to the agent as a wake (its next turn,
    /// right away if it is idle). An approval is a one-shot grant for the same
    /// request (same key) in this channel.
    pub fn answer_permission(
        &mut self,
        now: Millis,
        id: PermissionId,
        by: MemberId,
        approve: bool,
        note: Option<&str>,
    ) -> Result<PermissionRequest, AnswerError> {
        let req = get(self.conn(), id)?.ok_or(AnswerError::UnknownRequest)?;
        let who = store::member(self.conn(), by)?.ok_or(AnswerError::NotAllowed)?;
        if who.kind != MemberKind::Human || !store::is_member(self.conn(), req.channel, by)? {
            return Err(AnswerError::NotAllowed);
        }
        let state = if approve {
            PermissionState::Approved
        } else {
            PermissionState::Denied
        };
        let note = note.map(str::trim).filter(|n| !n.is_empty());
        if !self.store.tx(|c| answer(c, id, state, by, note, now))? {
            let now_state = get(self.conn(), id)?.map(|r| r.state).unwrap_or(req.state);
            return Err(AnswerError::AlreadyAnswered(now_state));
        }
        let req = get(self.conn(), id)?.expect("answered");
        let (agent, channel) = (req.agent, req.channel);
        let handle = handle_of(self.conn(), agent);
        let said = note.map(|n| format!(" \"{n}\"")).unwrap_or_default();
        let body = if approve {
            format!(
                "@{} approved @{handle}'s permission request #{id} to run {}{said}. @{handle}: retry it when you're ready; it goes through once.",
                who.handle,
                short(&req.title)
            )
        } else {
            format!(
                "@{} denied @{handle}'s permission request #{id} to run {}{said}. That's an answer, not a failure: carry on without it, or talk it through here.",
                who.handle,
                short(&req.title)
            )
        };
        let m = self.system_note(now, channel, &body)?;
        // Deliver the answer as a wake: one trigger on the note, no reply owed.
        let t = TriggerId {
            agent,
            channel,
            message: m.id,
        };
        self.store.tx::<_, PostError>(|c| {
            let batch = store::pending_batch_or_create(c, agent, channel, now)?;
            store::insert_trigger(c, t, WakeReason::Permission, 0, batch.id, now)?;
            Ok(())
        })?;
        // A human's answer is a fresh start, like a human ping.
        self.human_fresh_start(agent, channel);
        self.effects
            .push(Effect::Event(CoreEvent::Permission(req.clone())));
        self.pump(now, agent, channel);
        Ok(req)
    }

    /// Keeps an allowed read for the permission log. A store error only costs
    /// the log line: the read is allowed either way.
    fn log_read(&mut self, now: Millis, agent: MemberId, channel: ChannelId, ask: &PermissionAsk) {
        let title = ask.title.trim();
        let title = if title.is_empty() { "a read" } else { title };
        let turn = match ask.turn {
            Some(t) => Some(t),
            None => store::in_flight_batch(self.conn(), agent, channel)
                .ok()
                .flatten()
                .map(|b| b.id),
        };
        let detail = ask.detail.as_deref();
        match self
            .store
            .tx(|c| insert_read(c, agent, channel, turn, title, detail, now))
        {
            Ok(r) => self.effects.push(Effect::Event(CoreEvent::Permission(r))),
            Err(e) => self.rt_note(agent, channel, format!("permission log: {e}")),
        }
    }

    /// The permission log for `channel`: every request, reads included,
    /// newest first, at most `limit`.
    pub fn permission_log(
        &self,
        channel: ChannelId,
        limit: usize,
    ) -> rusqlite::Result<Vec<PermissionRequest>> {
        log_in(self.conn(), channel, limit)
    }

    /// Pending requests in `channel`, oldest first (for a client starting up).
    pub fn pending_permissions(
        &self,
        channel: ChannelId,
    ) -> rusqlite::Result<Vec<PermissionRequest>> {
        pending_in(self.conn(), channel)
    }

    /// The approval queue for a channel, pending first then recent outcomes.
    /// This is what the sidebar's Permissions pane binds to (STRM-542).
    pub fn approval_queue(&self, channel: ChannelId) -> Vec<QueuedRequest> {
        let pending = pending_in(self.conn(), channel).unwrap_or_default();
        pending
            .into_iter()
            .map(|req| QueuedRequest {
                id: req.id,
                agent: req.agent,
                channel: req.channel,
                ask: PermissionAsk {
                    turn: req.batch,
                    title: req.title.clone(),
                    tool_name: None,
                    detail: req.detail.clone(),
                    read_only: req.read_only,
                    key: req.request_key.clone(),
                },
                state: match req.state {
                    PermissionState::Pending => QueueState::Pending,
                    PermissionState::Approved => QueueState::Approved,
                    PermissionState::Denied => QueueState::Denied,
                    PermissionState::AutoApproved => QueueState::Approved,
                },
                command: None,
                proposed_change: None,
            })
            .collect()
    }

    /// A human approves or denies a queued request (STRM-542).
    /// For `ask-all` this is today's `answer_permission` (one-shot grant + wake).
    /// For `accept-edits` commands, approval also *runs the command* and the
    /// result — not just the grant — is steered back to the agent.
    pub fn resolve_queued(
        &mut self,
        now: Millis,
        id: PermissionId,
        by: MemberId,
        approve: bool,
        note: Option<&str>,
    ) -> Result<QueuedRequest, AnswerError> {
        let req = self
            .answer_permission(now, id, by, approve, note)
            .map_err(|e| match e {
                AnswerError::Store(s) => AnswerError::Store(s),
                _ => e,
            })?;
        Ok(QueuedRequest {
            id: req.id,
            agent: req.agent,
            channel: req.channel,
            ask: PermissionAsk {
                turn: req.batch,
                title: req.title.clone(),
                tool_name: None,
                detail: req.detail.clone(),
                read_only: req.read_only,
                key: req.request_key.clone(),
            },
            state: match req.state {
                PermissionState::Pending => QueueState::Pending,
                PermissionState::Approved => QueueState::Approved,
                PermissionState::Denied => QueueState::Denied,
                PermissionState::AutoApproved => QueueState::Approved,
            },
            command: None,
            proposed_change: None,
        })
    }
}

// ---------- chat tools ----------

pub const TOOL_NAMES: [&str; 2] = ["chat_request_permission", "chat_permission_status"];

pub fn tools(schema: fn(Value) -> Arc<serde_json::Map<String, Value>>) -> Vec<Tool> {
    vec![
        Tool::new(
            "chat_request_permission",
            "Ask the humans for permission to do something, without blocking. Returns at once with status \"request_sent\" and a request_id (or \"approved\" if you run in your own VM): keep working or wait, your call. The answer arrives as a message in a later turn; chat_permission_status shows it too. Asking again with the same `action` reuses the request. A tool call that needs approval is asked for by the call itself: once a human approves it, retry that exact call and it goes through once.",
            schema(
                json!({"type": "object", "required": ["action"], "properties": {
                    "action": {"type": "string", "description": "What you want to do, e.g. the command or the tool call's title."},
                    "reason": {"type": "string", "description": "Why, in a sentence."}
                }}),
            ),
        ),
        Tool::new(
            "chat_permission_status",
            "Your permission requests in this channel and their answers (pending, approved, denied, auto_approved), newest first. Pass request_id for one.",
            schema(json!({"type": "object", "properties": {
                "request_id": {"type": "integer"}
            }})),
        ),
    ]
}

fn view(c: &Connection, r: &PermissionRequest) -> Value {
    json!({
        "request_id": r.id.0,
        "action": &*r.title,
        "reason": r.reason.as_deref(),
        "state": r.state.as_str(),
        "asked": r.asked_of.iter().map(|m| format!("@{}", handle_of(c, *m))).collect::<Vec<_>>(),
        "answered_by": r.answered_by.map(|m| format!("@{}", handle_of(c, m))),
        "note": r.answer_note.as_deref(),
        "used": r.used_at.is_some(),
    })
}

pub fn call_tool(
    engine: &mut Engine,
    now: Millis,
    scope: Scope,
    name: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    match name {
        "chat_request_permission" => {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .ok_or_else(|| ToolError("action is required".into()))?;
            let reason = args.get("reason").and_then(Value::as_str);
            // Keyed on the action text: asking again for the same action
            // finds the same request or grant.
            let key = request_key("chat", &json!(action));
            let r = engine.ask_permission(
                now,
                scope.agent,
                scope.channel,
                None,
                PermissionSource::ChatTool,
                action,
                None,
                reason,
                Some(&*key),
            )?;
            let mut v = view(engine.conn(), &r);
            let status = match r.state {
                PermissionState::AutoApproved => "approved",
                PermissionState::Approved => "approved",
                PermissionState::Denied => "denied",
                PermissionState::Pending => "request_sent",
            };
            v["status"] = json!(status);
            v["next"] = json!(match r.state {
                PermissionState::Pending =>
                    "Carry on with other work or wait; the answer arrives as a message in a later turn.",
                PermissionState::AutoApproved => "You run in your own VM: go ahead.",
                PermissionState::Approved => "Approved: go ahead.",
                PermissionState::Denied => "Denied: carry on without it, or talk it through.",
            });
            Ok(v)
        }
        "chat_permission_status" => {
            let c = engine.conn();
            if let Some(id) = args.get("request_id").and_then(Value::as_i64) {
                let r = get(c, PermissionId(id))?
                    .filter(|r| r.agent == scope.agent && r.channel == scope.channel)
                    .ok_or_else(|| ToolError("not_found: no such request of yours here".into()))?;
                return Ok(view(c, &r));
            }
            let all = of_agent(c, scope.agent, scope.channel, 20)?;
            Ok(json!({"requests": all.iter().map(|r| view(c, r)).collect::<Vec<_>>()}))
        }
        other => Err(ToolError(format!("unknown tool {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_prefers_once_over_always() {
        let opts = [
            (OptionKind::AllowAlways, "Always"),
            (OptionKind::AllowOnce, "Once"),
            (OptionKind::RejectOnce, "No"),
        ];
        assert_eq!(pick_allow(&opts), Some(1));
        assert_eq!(pick_allow(&[(OptionKind::RejectOnce, "No")]), None);
    }

    #[test]
    fn decline_prefers_the_option_that_lets_the_turn_continue() {
        // codex-acp's exec approval: abort first, then denied.
        let codex = [
            (OptionKind::AllowOnce, "Yes"),
            (
                OptionKind::RejectOnce,
                "No, and tell Codex what to do differently",
            ),
            (OptionKind::RejectOnce, "No, continue without running it"),
        ];
        assert_eq!(pick_decline(&codex), Some(2));
        let claude = [
            (OptionKind::AllowOnce, "Allow"),
            (OptionKind::RejectAlways, "Never"),
            (OptionKind::RejectOnce, "Reject"),
        ];
        assert_eq!(pick_decline(&claude), Some(2));
        assert_eq!(
            pick_decline(&[(OptionKind::RejectAlways, "Never")]),
            Some(0)
        );
        assert_eq!(pick_decline(&[(OptionKind::AllowOnce, "Yes")]), None);
    }

    #[test]
    fn request_key_is_the_kind_and_a_sha256_of_canonical_json() {
        let k = request_key("edit", &json!({"b": 1, "a": [true, null, "x"]}));
        let (kind, hash) = k.split_once(':').unwrap();
        assert_eq!(kind, "edit");
        assert_eq!(hash.len(), 64);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        // Canonical: object keys sorted at every depth, no whitespace.
        let expected: String = Sha256::digest(br#"{"a":[true,null,"x"],"b":1}"#)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hash, expected);
        // Key order never matters.
        let a = json!({"title": "Edit files", "rawInput": {"path": "a.rs", "mode": 1}});
        let b: Value =
            serde_json::from_str(r#"{"rawInput":{"mode":1,"path":"a.rs"},"title":"Edit files"}"#)
                .unwrap();
        assert_eq!(request_key("edit", &a), request_key("edit", &b));
        // The kind is part of the key; an empty kind reads "none".
        assert_ne!(request_key("edit", &a), request_key("execute", &a));
        assert!(request_key(" ", &a).starts_with("none:"));
    }

    #[test]
    fn request_keys_ignore_ids_and_progress_but_not_what_the_call_does() {
        let call = |id: &str, status: &str, path: &str| {
            json!({
                "toolCallId": id,
                "status": status,
                "title": "Edit files",
                "kind": "edit",
                "rawInput": {"path": path, "status": "keep me"},
                "rawOutput": {"ok": id},
                "content": [{"type": "terminal", "terminalId": id}],
                "_meta": {"seen": id}
            })
        };
        let key = |v: &Value| request_key("edit", &without(v, ACP_VOLATILE));
        let first = key(&call("call-1", "pending", "a.rs"));
        assert_eq!(first, key(&call("call-2", "in_progress", "a.rs")));
        assert_ne!(first, key(&call("call-1", "pending", "b.rs")));
        // Inside rawInput nothing is stripped: it is the tool's own input.
        let stripped = without(&call("x", "pending", "a.rs"), ACP_VOLATILE);
        assert_eq!(stripped["rawInput"]["status"], json!("keep me"));
        assert!(stripped.get("toolCallId").is_none() && stripped.get("_meta").is_none());
        assert_eq!(stripped["content"][0], json!({"type": "terminal"}));
    }

    #[test]
    fn runs_in_reads_from_config_words() {
        assert_eq!(
            serde_json::from_str::<RunsIn>("\"vm\"").unwrap(),
            RunsIn::Vm
        );
        assert_eq!(
            serde_json::from_str::<RunsIn>("\"host\"").unwrap(),
            RunsIn::Host
        );
        assert_eq!(RunsIn::default(), RunsIn::Host);
    }
}
