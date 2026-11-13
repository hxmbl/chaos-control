//! Permissions by where the agent runs (STRM-401). The rules are written up in
//! `docs/agents.md` ("Permissions").
//!
//! - **On the host** (the user's computer) a mediated permission request goes
//!   to the humans and never blocks the agent. The agent hears "request sent"
//!   with a request id at once and decides whether to wait or do something
//!   else. A human's Approve or Deny arrives later, as a message that wakes the
//!   agent (its next turn). Nothing times out into an approval.
//! - **In the agent's own VM** ([`RunsIn::Vm`]) everything is approved: a broken
//!   VM is replaced, not protected.
//!
//! A protocol whose permission call blocks (ACP `session/request_permission`)
//! is answered at once: with the agent's own allow-once option when the
//! request is approved (VM, yolo/mode-allowed, or a one-shot grant a human
//! already gave for that exact request), else with its own decline ("not now")
//! option and a `_meta` note that a request was sent. An approval becomes a
//! one-shot grant: when the agent retries the exact same tool call in that
//! channel, it goes through once. "The same" is the request's key
//! ([`request_key`]): the tool kind plus a SHA-256 of what the call would do,
//! without its ids (STRM-572), so an approved `Edit files` on `a.rs` never
//! lets an `Edit files` on `b.rs` through, and a retry with a new tool call id
//! still matches. Agents can also ask ahead, and poll, through the chat tools
//! `chat_request_permission` and `chat_permission_status`.
//!
//! Peer model: a declined or denied request is an answer, not a failure. It
//! never turns an agent red or fails its turn.
//!
//! **Mediated ACP requests (STRM-596/606, partial):** on the production
//! `native_permission` path, requests are classified and decided fail-closed
//! (ACP mediation is not proof of complete mediation/enforcement). Generic ACP
//! kinds, tool basenames, titles, and self-declared metadata are never treated
//! as sufficient proof of safety or scope.
//!
//! - On the mediated host path, reads/local search, generic structured edits,
//!   delete/move, fetch/URL-open, execute/shell, unselected/unverified MCP, and
//!   unknown/changed/mixed/unclassifiable operations require human approval in
//!   both `ask-all` and `accept-edits` unless `yolo` allows everything
//!   (recorded as auto-approved-by-mode). Agents running in their own VM are
//!   auto-approved (`RunsIn::Vm`, recorded as auto-approved-by-VM). Execute in
//!   `accept-edits` is queued for approval-gated command execution (not
//!   auto-allowed); missing/uncapturable command/cwd fail closed for safe
//!   execution capture.
//! - Structured edits are never auto-allowed without verified editable-root
//!   authorization and sufficient execution-time scope enforcement evidence for
//!   the exact operation (unavailable in core today). Delete/move/mixed/
//!   unclassifiable operations are never auto-allow-eligible as structured edits.
//! - Request identity is bound by stable request keys where available; if
//!   identity cannot be stably bound, the request fails closed (no prior-grant
//!   reuse and no weak pending-key dedupe).
//! - CC control-plane: scoped native actions may be auto-approved only by exact
//!   tool identity (`chat_send`, `chat_react`, `chat_status`, `chat_refuse`,
//!   `chat_dissent`, `chat_request_permission`); CC read/status tools and
//!   policy/answer/unknown `chat_*` tools require human approval. This is
//!   separate from "selected MCP" allowances (which require verifiable proof not
//!   assumed here). No verified web-search/search-only or selected-MCP exceptions
//!   are granted without verifiable production-path evidence.
//!
//! **Approval modes (STRM-542, evolving under STRM-596/606).** On the mediated
//! host path, [`mode_verdict`] (with evidence/defense-in-depth guards) decides
//! handling: `ask-all` asks for mediated requests requiring approval;
//! `accept-edits` asks for reads/local search, structured edits, delete/move,
//! fetch, and other/mixed/unclassifiable requests and queues execute requests
//! for approval-gated execution (edits/delete/move are not auto-allowed without
//! verified prerequisites/evidence); `yolo` allows everything (recorded as
//! auto-approved-by-mode, `DecidedBy::AutoMode`). The effective mode resolves
//! channel > project > workspace > member `agents.json` `"approval"` (default
//! `ask-all`). Mode changes are not retroactive.

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
    /// Captured command line for queued/runnable commands (STRM-542).
    pub command: Option<Arc<str>>,
    /// Working directory for command execution (STRM-542).
    pub cwd: Option<Arc<str>>,
    /// Execution lifecycle state: queued/running/done/failed (STRM-542).
    pub run_state: Option<Arc<str>>,
    /// Captured command output/result summary (STRM-542).
    pub run_output: Option<Arc<str>>,
    /// Exit code when execution finished (STRM-542).
    pub run_exit_code: Option<i32>,
    /// When execution finished (STRM-542).
    pub run_finished_at: Option<Millis>,
}

/// How a request ended, as the permission log shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogOutcome {
    /// Allowed without a human: a read, or anything from an agent in its own VM.
    AutoAllowed,
    /// Waiting for a human decision (live queue, STRM-542).
    Pending,
    /// A human denied it.
    Declined,
    /// A human approved it (a one-shot grant).
    Approved,
    /// Approved command currently running (STRM-542).
    Running,
    /// Queued command completed (STRM-542).
    Done,
    /// Queued command failed or could not be executed (STRM-542).
    Failed,
}

impl LogOutcome {
    pub fn label(self) -> &'static str {
        match self {
            LogOutcome::AutoAllowed => "Auto-allowed",
            LogOutcome::Pending => "Pending",
            LogOutcome::Declined => "Declined",
            LogOutcome::Approved => "Approved",
            LogOutcome::Running => "Running",
            LogOutcome::Done => "Done",
            LogOutcome::Failed => "Failed",
        }
    }
}

impl PermissionRequest {
    /// The outcome the permission log/queue shows.
    pub fn outcome(&self) -> LogOutcome {
        match self.run_state.as_deref() {
            Some("running") => return LogOutcome::Running,
            Some("done") => return LogOutcome::Done,
            Some("failed") | Some("unrunnable") => return LogOutcome::Failed,
            Some("denied") => return LogOutcome::Declined,
            Some("queued") => return LogOutcome::Pending,
            _ => {}
        }
        match self.state {
            PermissionState::AutoApproved => LogOutcome::AutoAllowed,
            PermissionState::Approved => LogOutcome::Approved,
            PermissionState::Denied => LogOutcome::Declined,
            PermissionState::Pending => LogOutcome::Pending,
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
    /// What kind of operation this is (STRM-542). Used for mode decisions.
    pub kind: RequestKind,
    /// The request's key ([`request_key`]), which a grant must match. None
    /// falls back to a key on the title alone.
    pub key: Option<Arc<str>>,
}

/// How the adapter must answer a native permission request, right away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionVerdict {
    /// Pick the agent's own allow-once option (VM agent, or a human's grant).
    Allow { request: PermissionId },
    /// Pick the agent's own allow-once option for a read-only tool call when
    /// policy allows it. (Not used for ACP reads/local search on the mediated
    /// STRM-606 path, which require approval in ask-all/accept-edits.)
    AllowRead,
    /// Pick the agent's own decline ("not now") option; the request went to
    /// the humans and the answer arrives later.
    Sent { request: PermissionId },
    /// No core to ask (replay tests): decline, nothing recorded.
    Decline,
}

/// What kind of operation a permission request is for (STRM-542/STRM-606).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// A read-only operation (file read, search).
    Read,
    /// A structured edit operation (file write/change).
    Edit,
    /// A delete operation.
    Delete,
    /// A move/rename operation.
    Move,
    /// A command execution.
    Execute,
    /// A fetch operation (HTTP, etc.).
    Fetch,
    /// Some other, mixed, or unclassifiable operation.
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
    Unified { path: Arc<str>, patch: Arc<str> },
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
        "Request #{request} sent to Chaos Control's approval queue. You can continue other work. \
Check status with chat_permission_status (request_id: {request}); you'll be notified when a decision is made. \
If approved, follow the decision message (you may need to retry the same call once; queued commands may be run and reported back by Chaos Control as support is added)."
    )
}

/// Whether an ACP tool kind (as its wire string: `read`, `search`, ...) is a
/// read/local-search-shaped kind. Classification alone does not imply the
/// operation is auto-allowed under current mediated policy.
pub fn is_read_only_kind(kind: &str) -> bool {
    matches!(kind, "read" | "search")
}

/// Maps an ACP wire tool kind to a [`RequestKind`] (STRM-542/STRM-606).
/// Delete/move are classified explicitly (never as auto-allow-eligible edits);
/// unknown/missing/unsupported kinds (including `switch_mode`, `think`, and any
/// future/unrecognized kind) are treated as other/unclassifiable and fail closed.
pub fn request_kind_from_wire(kind: Option<&str>) -> RequestKind {
    match kind.map(str::trim) {
        Some("read" | "search") => RequestKind::Read,
        Some("edit") => RequestKind::Edit,
        Some("delete") => RequestKind::Delete,
        Some("move") => RequestKind::Move,
        Some("execute") => RequestKind::Execute,
        Some("fetch") => RequestKind::Fetch,
        Some("") | None => RequestKind::Other,
        Some(_) => RequestKind::Other,
    }
}

/// True when `name` is an exact CC control-plane chat tool name eligible for
/// scoped native auto-approval on the mediated path (separate from "selected
/// MCP" allowances). Read/status and policy/answer tools are excluded and must
/// require human approval. Identity matching is exact (no normalization).
pub fn is_auto_allowed_cc_control_plane_tool(name: &str) -> bool {
    AUTO_ALLOWED_CHAT_TOOL_NAMES.contains(&name)
}

/// Execution-time evidence for a mediated structured edit on the production
/// path (STRM-596/606). Auto-allow eligibility in `accept-edits` requires
/// explicit editable-root authorization and sufficient execution-time scope
/// enforcement evidence for the exact operation. Core has no verified evidence
/// on this path today and must fail closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum StructuredEditEvidence {
    /// No verified editable-root/scope-enforcement evidence available.
    #[default]
    Unverified,
    /// Verified prerequisites are satisfied for this exact operation.
    /// (Not constructible on the current core production path.)
    #[doc(hidden)]
    _Verified,
}

impl StructuredEditEvidence {
    /// Whether verified prerequisites allow a structured edit to be considered
    /// for auto-allow in `accept-edits` (subject to mode and kind rules).
    pub fn allows_auto_edit(self) -> bool {
        matches!(self, Self::_Verified)
    }
}

/// Shell/execute operations are never auto-allow-eligible on the mediated path
/// outside `yolo`; `accept-edits` uses approval-gated queuing (not auto-allow).
pub fn execute_auto_allow_eligible_without_yolo() -> bool {
    false
}

/// Verified prerequisites required before a mediated structured edit could be
/// auto-allow-eligible in `accept-edits` on the production path (STRM-596/606):
/// explicit editable-root authorization and sufficient execution-time scope
/// enforcement evidence for the exact operation. Core does not have verified
/// evidence on this path today; callers must fail closed.
pub fn structured_edit_prerequisites_verified() -> bool {
    StructuredEditEvidence::Unverified.allows_auto_edit()
}

/// Conservative fail-closed refinement for mediated classification: if an
/// `Edit`-classified request shows verifiable protocol/detail evidence of a
/// conflicting delete/move operation kind (or other unclassifiable mixed
/// signal) in its structured payload, treat it as `Other`/unclassifiable so it
/// can never be considered an auto-allow-eligible structured edit. This uses
/// only structural kind markers present in serialized request details; absence
/// of evidence is not treated as proof of safety.
pub fn effective_request_kind(kind: RequestKind, detail: Option<&str>) -> RequestKind {
    if kind != RequestKind::Edit {
        return kind;
    }
    let Some(v) = detail.and_then(|d| serde_json::from_str::<Value>(d).ok()) else {
        return kind;
    };
    if has_conflicting_edit_kind_signal(&v) {
        RequestKind::Other
    } else {
        kind
    }
}

fn has_conflicting_edit_kind_signal(v: &Value) -> bool {
    match v {
        Value::Object(m) => {
            // A structured operation-kind marker inside an edit-classified payload
            // that indicates a concrete conflicting/mixed operation shape must fail
            // closed. `edit`/`read`/`search` markers are not treated as
            // conflicting here; absence of evidence is not proof of a pure
            // structured edit. Ambiguous generic markers like `other`/`unknown`
            // are intentionally not used as signals to avoid false positives on
            // arbitrary payload data.
            if let Some(Value::String(k)) = m.get("kind")
                && matches!(
                    k.as_str(),
                    "delete" | "move" | "execute" | "fetch" | "switch_mode"
                )
            {
                return true;
            }
            m.values().any(has_conflicting_edit_kind_signal)
        }
        Value::Array(a) => a.iter().any(has_conflicting_edit_kind_signal),
        _ => false,
    }
}

/// What a mode does with one incoming request, before any human sees it.
/// Pure function of (mode, request kind) — no I/O, fully testable (STRM-542/STRM-606).
///
/// Contract (STRM-596, mediated): ordinary/private/out-of-workspace reads and
/// local search require human approval in both `ask-all` and `accept-edits`.
/// Generic structured edit requests are not auto-approved in `accept-edits` on
/// this path until structured-edit/editable-root and sufficient execution-time
/// scope enforcement evidence are available in production; absent those
/// prerequisites, fail closed to human approval. Delete, move, and
/// other/mixed/unclassifiable operations require human approval in both modes
/// and are never treated as auto-allow-eligible edits. Fetch and
/// unknown/other operations require human approval in both modes. Execute
/// (shell/commands, including read-only-looking commands) requires human
/// approval in both modes; in `accept-edits` it is queued for approval-gated
/// command execution (preserved behavior) rather than auto-allowed. `yolo`
/// allows everything.
pub fn mode_verdict(mode: ApprovalMode, kind: RequestKind) -> ModeOutcome {
    mode_verdict_with_edit_evidence(mode, kind, StructuredEditEvidence::Unverified)
}

/// Like [`mode_verdict`], but parameterized by verified structured-edit
/// evidence. No verified evidence exists on the current core production path;
/// callers must pass [`StructuredEditEvidence::Unverified`] unless they can
/// prove prerequisites for the exact operation.
pub fn mode_verdict_with_edit_evidence(
    mode: ApprovalMode,
    kind: RequestKind,
    edit_evidence: StructuredEditEvidence,
) -> ModeOutcome {
    match mode {
        ApprovalMode::AskAll => ModeOutcome::AskHuman,
        ApprovalMode::AcceptEdits => match kind {
            RequestKind::Read
            | RequestKind::Edit
            | RequestKind::Delete
            | RequestKind::Move
            | RequestKind::Fetch
            | RequestKind::Other => {
                // Only a pure structured edit with verified prerequisites may
                // become auto-allow-eligible; delete/move/mixed/unclassifiable
                // never qualify.
                if kind == RequestKind::Edit && edit_evidence.allows_auto_edit() {
                    ModeOutcome::Allow
                } else {
                    ModeOutcome::AskHuman
                }
            }
            RequestKind::Execute => ModeOutcome::Queue,
        },
        ApprovalMode::Yolo => ModeOutcome::Allow,
    }
}

// ---------- request keys ----------

/// The fields of an ACP tool call that differ between two calls of the same
/// thing: its ids, its progress and what it produced. Left out of the key.
pub const ACP_VOLATILE: &[&str] = &["toolCallId", "terminalId", "status", "rawOutput", "_meta"];

fn detail_json(detail: Option<&str>) -> Option<Value> {
    detail.and_then(|d| serde_json::from_str(d).ok())
}

/// Best-effort extraction of an executable shell command from stored request
/// detail (STRM-542). Returns None when no safe command string can be inferred.
pub fn extract_command(detail: Option<&str>) -> Option<String> {
    let v = detail_json(detail)?;
    fn find(v: &Value) -> Option<String> {
        if let Some(obj) = v.as_object() {
            for key in ["command", "cmd", "script", "shell"] {
                if let Some(c) = obj.get(key) {
                    match c {
                        Value::String(s) if !s.trim().is_empty() => return Some(s.clone()),
                        Value::Array(a) => {
                            let mut parts = Vec::with_capacity(a.len());
                            for x in a {
                                let s = x.as_str()?;
                                parts.push(s.to_string());
                            }
                            if !parts.is_empty() {
                                return Some(shell_join(&parts));
                            }
                        }
                        Value::Object(o) => {
                            // Some shapes nest the actual command, e.g. {command:{command:"..."}}
                            if let Some(found) = find(&Value::Object(o.clone())) {
                                return Some(found);
                            }
                        }
                        _ => {}
                    }
                }
            }
            // ACP v2/tool-call-like nested locations
            for key in [
                "rawInput",
                "input",
                "arguments",
                "params",
                "toolCall",
                "subject",
                "tool_call",
                "fields",
                "content",
            ] {
                if let Some(inner) = obj.get(key)
                    && let Some(found) = find(inner)
                {
                    return Some(found);
                }
            }
            // Some ACP tool calls put command in title/description-like fields? no, avoid guessing.
        } else if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(found) = find(item) {
                    return Some(found);
                }
            }
        }
        None
    }
    find(&v)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Best-effort extraction of a working directory from request detail.
pub fn extract_cwd(detail: Option<&str>) -> Option<String> {
    let v = detail_json(detail)?;
    fn find(v: &Value) -> Option<String> {
        if let Some(obj) = v.as_object() {
            for key in [
                "cwd",
                "workdir",
                "working_directory",
                "workingDirectory",
                "directory",
                "dir",
            ] {
                if let Some(Value::String(s)) = obj.get(key) {
                    let s = s.trim();
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
            for key in [
                "rawInput",
                "input",
                "arguments",
                "params",
                "toolCall",
                "subject",
                "tool_call",
                "fields",
            ] {
                if let Some(inner) = obj.get(key)
                    && let Some(found) = find(inner)
                {
                    return Some(found);
                }
            }
        } else if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(found) = find(item) {
                    return Some(found);
                }
            }
        }
        None
    }
    find(&v)
}

fn shell_join(parts: &[String]) -> String {
    // If this looks like ["bash","-lc","<script>"] or ["sh","-c","<script>"], prefer the script.
    if parts.len() >= 3 {
        let shell = parts[0].as_str();
        let flag = parts[1].as_str();
        if matches!(
            shell,
            "bash" | "sh" | "zsh" | "/bin/bash" | "/bin/sh" | "/bin/zsh"
        ) && matches!(flag, "-lc" | "-c")
        {
            return parts[2].clone();
        }
    }
    // Otherwise join as a simple command line (best-effort).
    parts
        .iter()
        .map(|p| {
            if p.chars()
                .any(|c| c.is_whitespace() || "\"'\\$`".contains(c))
            {
                format!("'{}'", p.replace('\'', "'\\''"))
            } else {
                p.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

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
asked_of, created_at, answered_by, answered_at, answer_note, used_at, read_only, request_key, decided_by, \
command, cwd, run_state, run_output, run_exit_code, run_finished_at";

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
        command: r.get::<_, Option<String>>(18)?.map(Arc::from),
        cwd: r.get::<_, Option<String>>(19)?.map(Arc::from),
        run_state: r.get::<_, Option<String>>(20)?.map(Arc::from),
        run_output: r.get::<_, Option<String>>(21)?.map(Arc::from),
        run_exit_code: r.get::<_, Option<i64>>(22)?.map(|v| v as i32),
        run_finished_at: r.get::<_, Option<i64>>(23)?.map(|t| t as Millis),
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
    /// Captured command for queued execution (STRM-542).
    pub command: Option<&'a str>,
    /// Working directory for queued execution (STRM-542).
    pub cwd: Option<&'a str>,
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
             created_at, read_only, request_key, decided_by, command, cwd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
            r.decided_by.as_ref().map(|d| d.as_str()),
            r.command,
            r.cwd,
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

/// Requests relevant to the approval/command queue for a channel (STRM-542):
/// pending, answered with run lifecycle, or otherwise actionable/recent.
pub fn queue_in(
    c: &Connection,
    channel: ChannelId,
    limit: usize,
) -> rusqlite::Result<Vec<PermissionRequest>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {COLS} FROM permission_requests
         WHERE channel_id = ?1 AND read_only = 0
           AND (state = 'pending' OR run_state IS NOT NULL OR state = 'denied' OR state = 'approved')
         ORDER BY
           CASE
             WHEN run_state = 'running' THEN 0
             WHEN state = 'pending' THEN 1
             WHEN run_state = 'queued' THEN 2
             WHEN run_state IN ('done','failed','denied','unrunnable') THEN 3
             ELSE 4
           END,
           id DESC
         LIMIT ?2"
    ))?;
    stmt.query_map(params![channel.0, limit as i64], row)?
        .collect()
}

fn queue_state_of(req: &PermissionRequest) -> QueueState {
    match req.run_state.as_deref() {
        Some("running") => QueueState::Approved,
        Some("done") => QueueState::Done,
        Some("failed") | Some("unrunnable") => QueueState::Failed,
        Some("denied") => QueueState::Denied,
        Some("queued") => QueueState::Pending,
        _ => match req.state {
            PermissionState::Pending => QueueState::Pending,
            PermissionState::Approved | PermissionState::AutoApproved => QueueState::Approved,
            PermissionState::Denied => QueueState::Denied,
        },
    }
}

/// Queued commands left running by a previous process (startup recovery).
pub fn interrupted_runs(c: &Connection) -> rusqlite::Result<Vec<PermissionRequest>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {COLS} FROM permission_requests WHERE run_state = 'running' ORDER BY id"
    ))?;
    stmt.query_map([], row)?.collect()
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
            command: None,
            cwd: None,
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
        params![
            id.0,
            state.as_str(),
            by.0,
            now as i64,
            note,
            decided_by.as_str()
        ],
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

pub fn set_run_state(c: &Connection, id: PermissionId, state: &str) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE permission_requests SET run_state = ?2 WHERE id = ?1",
        params![id.0, state],
    )?;
    Ok(())
}

pub fn set_run_result(
    c: &Connection,
    id: PermissionId,
    state: &str,
    output: Option<&str>,
    exit_code: Option<i32>,
    finished_at: Millis,
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE permission_requests SET run_state = ?2, run_output = ?3, run_exit_code = ?4, run_finished_at = ?5 WHERE id = ?1",
        params![id.0, state, output, exit_code, exit_code.map(|_| finished_at as i64).or(Some(finished_at as i64))],
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

    /// Sets the host-side approval mode for `agent` (STRM-542). Persisted.
    pub fn set_approval_mode(
        &mut self,
        agent: MemberId,
        mode: ApprovalMode,
    ) -> rusqlite::Result<()> {
        let key = format!("member_approval_mode:{}", agent.0);
        self.store
            .tx(|c| crate::store::set_app_setting(c, &key, Some(mode.as_str())))?;
        self.approval_mode.insert(agent, mode);
        Ok(())
    }

    pub fn approval_mode(&self, agent: MemberId) -> ApprovalMode {
        let key = format!("member_approval_mode:{}", agent.0);
        match crate::store::app_setting(self.conn(), &key) {
            Ok(Some(m)) => ApprovalMode::parse(&m),
            Ok(None) => self
                .approval_mode
                .get(&agent)
                .copied()
                .unwrap_or(ApprovalMode::AskAll),
            Err(_) => ApprovalMode::AskAll,
        }
    }

    /// Records the agent's working directory for command execution.
    pub fn set_agent_cwd(&mut self, agent: MemberId, cwd: std::path::PathBuf) {
        self.agent_cwd.insert(agent, cwd);
    }

    pub fn agent_cwd(&self, agent: MemberId) -> Option<std::path::PathBuf> {
        self.agent_cwd.get(&agent).cloned()
    }

    /// Effective approval mode for a new request in `channel` (STRM-542):
    /// channel override > project override > workspace override > member default.
    /// Fails closed to `ask-all` if a higher-priority override cannot be read.
    pub fn effective_approval_mode(&self, agent: MemberId, channel: ChannelId) -> ApprovalMode {
        match crate::store::channel_approval_mode(self.conn(), channel) {
            Ok(Some(m)) => return ApprovalMode::parse(&m),
            Err(_) => return ApprovalMode::AskAll,
            Ok(None) => {}
        }
        let project = match crate::store::channel(self.conn(), channel) {
            Ok(Some(ch)) => Some(ch.project),
            Ok(None) => None,
            Err(_) => return ApprovalMode::AskAll,
        };
        if let Some(project) = project {
            match crate::store::project_setting(self.conn(), &project, "approval_mode") {
                Ok(Some(m)) => return ApprovalMode::parse(&m),
                Err(_) => return ApprovalMode::AskAll,
                Ok(None) => {}
            }
        }
        match crate::store::app_setting(self.conn(), "approval_mode") {
            Ok(Some(m)) => ApprovalMode::parse(&m),
            Ok(None) => self.approval_mode(agent),
            Err(_) => ApprovalMode::AskAll,
        }
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
            .filter(|name| is_auto_allowed_cc_control_plane_tool(name))
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
        // STRM-596/606: URL fetch/open, unselected MCP, and unknown/changed
        // identities must be asked in both modes. Verified web-search (search-only)
        // and explicitly selected MCP allowances require verifiable proof on the
        // production path; without that proof, no automatic exception is granted.
        // Generic ACP kinds, tool basenames, titles, and self-declared metadata
        // are never sufficient (fail closed). Conflicting/mixed edit-payload
        // signals are treated as unclassifiable (Ask in both modes).
        let kind = effective_request_kind(ask.kind, ask.detail.as_deref());
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
            kind,
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
        if !is_auto_allowed_cc_control_plane_tool(name) {
            // Defense-in-depth: only exact scoped CC control-plane actions may be
            // auto-approved on this path; everything else must fail closed.
            return self.ask_permission(
                now,
                agent,
                channel,
                turn,
                PermissionSource::Native,
                name,
                detail,
                None,
                None,
                RequestKind::Other,
            );
        }
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
                    decided_by: Some(DecidedBy::AutoMode),
                    command: None,
                    cwd: None,
                },
            )
        })?;
        self.rt_note(
            agent,
            channel,
            format!("using {} (chat tool, auto-allowed)", short(name)),
        );
        self.effects
            .push(Effect::Event(Box::new(CoreEvent::Permission(r.clone()))));
        Ok(r)
    }

    /// Records a request and works out its state: auto-approved in a VM, the
    /// grant a human already gave (used up here), the same request already
    /// pending, or a new pending request for the humans. "The same" means the
    /// same `key` ([`request_key`]). If the request identity cannot be stably
    /// bound (no key), fail closed on the mediated path: do not reuse/consume a
    /// prior grant and do not dedupe as "the same request" via a weak fallback;
    /// always require an explicit decision/approval for this ask.
    /// Policy/mode classification is applied on the mediated production path
    /// (STRM-606); reads/local search and other protected classes are not
    /// auto-allowed by bypassing this decision.
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
        kind: RequestKind,
    ) -> Result<PermissionRequest, PostError> {
        let title = title.trim();
        let title = if title.is_empty() { "a tool" } else { title };
        let (key, bindable): (Arc<str>, bool) = match key.map(str::trim).filter(|k| !k.is_empty()) {
            Some(k) => (Arc::from(k), true),
            // Unbindable identity: derive a per-ask key so this request cannot
            // match/consume an unrelated prior grant or be collapsed with a
            // different unverified request (fail closed).
            None => (
                request_key(
                    "unbindable",
                    &json!({
                        "title": title,
                        "detail": detail,
                        "reason": reason,
                        "source": source.as_str(),
                        "kind": format!("{kind:?}"),
                        "now": now,
                    }),
                ),
                false,
            ),
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
                        command: None,
                        cwd: None,
                    },
                )
            })?;
            self.rt_note(
                agent,
                channel,
                format!("ran {} (own VM, auto-approved)", short(title)),
            );
            self.effects
                .push(Effect::Event(Box::new(CoreEvent::Permission(r.clone()))));
            return Ok(r);
        }
        // STRM-542/596/606: apply host-side approval mode. Defense-in-depth on
        // the mediated path: delete/move must never be auto-allowed as edits, and
        // structured edits may only become auto-allow-eligible in accept-edits
        // with verified editable-root + execution-time enforcement prerequisites/
        // evidence on the production path (none here). `yolo` explicitly allows
        // everything. Fail closed outside `yolo`.
        let mode = self.effective_approval_mode(agent, channel);
        let outcome =
            mode_verdict_with_edit_evidence(mode, kind, StructuredEditEvidence::Unverified);
        let outcome = match (outcome, kind) {
            (ModeOutcome::Allow, RequestKind::Edit | RequestKind::Delete | RequestKind::Move)
                if mode != ApprovalMode::Yolo =>
            {
                ModeOutcome::AskHuman
            }
            // Execute must never be silently auto-allowed outside yolo on the
            // mediated path (ask-all asks; accept-edits queues with human approval).
            (ModeOutcome::Allow, RequestKind::Execute) if mode != ApprovalMode::Yolo => {
                ModeOutcome::AskHuman
            }
            (other, _) => other,
        };
        if outcome == ModeOutcome::Allow {
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
                        decided_by: Some(DecidedBy::AutoMode),
                        command: None,
                        cwd: None,
                    },
                )
            })?;
            let mode_label = mode.as_str();
            self.rt_note(
                agent,
                channel,
                format!("ran {} (auto-approved, {mode_label})", short(title)),
            );
            self.effects
                .push(Effect::Event(Box::new(CoreEvent::Permission(r.clone()))));
            return Ok(r);
        }
        if bindable {
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
                    .push(Effect::Event(Box::new(CoreEvent::Permission(r.clone()))));
                return Ok(r);
            }
            if let Some(r) = pending_for(self.conn(), agent, channel, &key)? {
                return Ok(r);
            }
        }
        let asked_of = self.route(channel, turn);
        let queued = outcome == ModeOutcome::Queue;
        let command = if queued && kind == RequestKind::Execute {
            extract_command(detail)
        } else {
            None
        };
        let (req_cwd, run_state_seed) = if queued && kind == RequestKind::Execute {
            match command.as_deref() {
                Some(_) => {
                    let resolved = extract_cwd(detail).or_else(|| {
                        self.agent_cwd(agent)
                            .map(|p| p.to_string_lossy().into_owned())
                    });
                    match resolved {
                        Some(cwd) => (Some(cwd), Some("queued")),
                        None => (None, Some("unrunnable")),
                    }
                }
                None => (None, Some("unrunnable")),
            }
        } else {
            (None, None)
        };
        let r = self.store.tx(|c| {
            let r = insert(
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
                    command: command.as_deref(),
                    cwd: req_cwd.as_deref(),
                },
            )?;
            if let Some(state) = run_state_seed {
                c.execute(
                    "UPDATE permission_requests SET run_state = ?2 WHERE id = ?1",
                    params![r.id.0, state],
                )?;
            }
            Ok::<_, rusqlite::Error>(r)
        })?;
        let r = get(self.conn(), r.id)?.expect("inserted");
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
            let verb = if queued { "queued" } else { "asked to run" };
            let tail = if queued {
                if r.run_state.as_deref() == Some("unrunnable") {
                    " Command details could not be safely captured for execution by Chaos Control; if approved, you may need to retry/run it yourself when supported. Check chat_permission_status for status.".to_string()
                } else {
                    " You can continue working; check status with chat_permission_status and you'll be told when a decision is made.".to_string()
                }
            } else {
                format!(" Any human here can approve or deny it; @{handle} carries on meanwhile.")
            };
            self.system_note(
                now,
                channel,
                &format!(
                    "@{handle} {verb} {}{why}: permission request #{}{to}.{tail}",
                    short(title),
                    r.id
                ),
            )?;
        }
        self.effects
            .push(Effect::Event(Box::new(CoreEvent::Permission(r.clone()))));
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
        let mut req = get(self.conn(), id)?.expect("answered");
        let was_queued_cmd = req.run_state.as_deref() == Some("queued")
            && req.command.is_some()
            && req.cwd.is_some();
        let unrunnable_queued = req.run_state.as_deref() == Some("unrunnable")
            || (req.run_state.as_deref() == Some("queued")
                && (req.command.is_none() || req.cwd.is_none()));
        let runnable = approve && was_queued_cmd;
        if approve && req.run_state.as_deref() == Some("queued") && !was_queued_cmd {
            self.store.tx(|c| set_run_state(c, id, "unrunnable"))?;
            req = get(self.conn(), id)?.expect("answered");
        }
        if runnable {
            self.store.tx(|c| set_run_state(c, id, "running"))?;
            req = get(self.conn(), id)?.expect("answered");
        } else if !approve && (req.run_state.as_deref() == Some("queued") || unrunnable_queued) {
            self.store.tx(|c| set_run_state(c, id, "denied"))?;
            req = get(self.conn(), id)?.expect("answered");
        }
        let (agent, channel) = (req.agent, req.channel);
        let handle = handle_of(self.conn(), agent);
        let said = note.map(|n| format!(" \"{n}\"")).unwrap_or_default();
        let body = if approve {
            if runnable {
                format!(
                    "@{} approved @{handle}'s queued command #{id} ({}).{said} Running it now; @{handle}: you'll be notified with the result when it finishes. Check chat_permission_status(request_id: {id}) for status/output.",
                    who.handle,
                    short(&req.title)
                )
            } else if unrunnable_queued {
                format!(
                    "@{} approved @{handle}'s queued request #{id} to run {}{said}, but Chaos Control could not safely capture a runnable command. @{handle}: retry the same action when you're ready if supported; it may go through once.",
                    who.handle,
                    short(&req.title)
                )
            } else {
                format!(
                    "@{} approved @{handle}'s permission request #{id} to run {}{said}. @{handle}: retry it when you're ready; it goes through once.",
                    who.handle,
                    short(&req.title)
                )
            }
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
            .push(Effect::Event(Box::new(CoreEvent::Permission(req.clone()))));
        self.pump(now, agent, channel);
        Ok(req)
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

    /// The approval queue for a channel: pending, running, and recent outcomes.
    /// This is what the sidebar's Permissions pane binds to (STRM-542).
    pub fn approval_queue(&self, channel: ChannelId) -> rusqlite::Result<Vec<QueuedRequest>> {
        Ok(queue_in(self.conn(), channel, 100)?
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
                    kind: if req.read_only {
                        RequestKind::Read
                    } else {
                        RequestKind::Other
                    },
                    key: req.request_key.clone(),
                },
                state: queue_state_of(&req),
                command: req.command.clone(),
                proposed_change: None,
            })
            .collect())
    }

    /// Records completion of a Chaos-Control-executed queued command and steers
    /// the result to the agent (STRM-542). Persists result and notification together.
    pub fn complete_command_run(
        &mut self,
        now: Millis,
        id: PermissionId,
        ok: bool,
        exit_code: Option<i32>,
        output: &str,
    ) -> Result<PermissionRequest, AnswerError> {
        let req = get(self.conn(), id)?.ok_or(AnswerError::UnknownRequest)?;
        if req.run_state.as_deref() != Some("running") {
            return Err(AnswerError::Store("command is not running".into()));
        }
        let state = if ok { "done" } else { "failed" };
        let output = output.trim();
        let out_store = if output.is_empty() {
            None
        } else {
            Some(output)
        };
        let (agent, channel) = (req.agent, req.channel);
        let handle = handle_of(self.conn(), agent);
        let status = match (ok, exit_code) {
            (true, Some(code)) => format!("completed (exit {code})"),
            (true, None) => "completed".to_string(),
            (false, Some(code)) => format!("failed (exit {code})"),
            (false, None) => "failed".to_string(),
        };
        let shown = if output.is_empty() {
            "(no output)".to_string()
        } else {
            const MAX: usize = 4000;
            if output.chars().count() > MAX {
                let mut s: String = output.chars().take(MAX).collect();
                s.push_str("\n…(truncated)");
                s
            } else {
                output.to_string()
            }
        };
        let body = format!(
            "@{handle} queued command #{id} ({}) {status}.\nOutput:\n{shown}",
            short(&req.title)
        );
        let (req, msg) = self.store.tx::<_, rusqlite::Error>(|c| {
            set_run_result(c, id, state, out_store, exit_code, now)?;
            let msg = store::insert_message(
                c,
                &crate::store::NewMessage {
                    channel,
                    author: None,
                    origin: Origin::System,
                    body: &body,
                    reply_to: None,
                    mentions: &[],
                    hop: 0,
                    cause_batch: None,
                    client_request_id: None,
                    sent_at: now,
                },
            )?;
            let batch = store::pending_batch_or_create(c, agent, channel, now)?;
            let t = TriggerId {
                agent,
                channel,
                message: msg.id,
            };
            store::insert_trigger(c, t, WakeReason::Permission, 0, batch.id, now)?;
            let req = get(c, id)?.expect("updated");
            Ok((req, msg))
        })?;
        self.effects
            .push(Effect::Event(Box::new(CoreEvent::Message(msg))));
        self.human_fresh_start(agent, channel);
        self.effects
            .push(Effect::Event(Box::new(CoreEvent::Permission(req.clone()))));
        self.pump(now, agent, channel);
        Ok(req)
    }

    /// Marks queued commands left `running` by a previous process as failed
    /// and notifies the agent (startup recovery).
    pub fn recover_interrupted_command_runs(&mut self, now: Millis) {
        let Ok(runs) = interrupted_runs(self.conn()) else {
            return;
        };
        for req in runs {
            let _ = self.complete_command_run(
                now,
                req.id,
                false,
                None,
                "interrupted: Chaos Control restarted while the command was running",
            );
        }
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
                kind: if req.read_only {
                    RequestKind::Read
                } else {
                    RequestKind::Other
                },
                key: req.request_key.clone(),
            },
            state: queue_state_of(&req),
            command: req.command.clone(),
            proposed_change: None,
        })
    }
}

// ---------- chat tools ----------

pub const TOOL_NAMES: [&str; 4] = [
    "chat_request_permission",
    "chat_permission_status",
    "chat_approval_mode_status",
    "chat_set_approval_mode",
];

/// CC-internal control-plane chat tools that may be auto-approved without
/// per-call human approval when requested natively, but only as exact
/// CC-owned tool identities on the mediated path (separate from "explicitly
/// selected MCP" allowances; STRM-596/606 v3). Read/status control-plane tools
/// (`chat_context`, `chat_read`, `chat_search`, `chat_permission_status`,
/// `chat_approval_mode_status`) require human approval in both modes and are
/// therefore excluded. Policy/authorization actions remain separately
/// controlled and must not be agent-self-approved.
pub const AUTO_ALLOWED_CHAT_TOOL_NAMES: [&str; 6] = [
    "chat_send",
    "chat_react",
    "chat_refuse",
    "chat_dissent",
    "chat_status",
    "chat_request_permission",
];

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
            "Your permission/queue requests in this channel and their state (pending, approved, denied, auto_approved) plus queued command run info when available (run_state: queued/running/done/failed/unrunnable/denied, output, exit code). Newest first. Pass request_id to watch one.",
            schema(json!({"type": "object", "properties": {
                "request_id": {"type": "integer"}
            }})),
        ),
        Tool::new(
            "chat_approval_mode_status",
            "Show the effective approval mode for new requests in this channel and configured overrides (member/workspace/project/channel).",
            schema(json!({"type": "object", "properties": {}})),
        ),
        Tool::new(
            "chat_set_approval_mode",
            "Set or clear an approval mode override. Scope: 'channel', 'project', 'workspace', or 'member' (default member). Mode: 'ask-all', 'accept-edits', 'yolo', or null to clear an override (member cannot be cleared). Affects new requests only.",
            schema(
                json!({"type": "object", "required": ["mode"], "properties": {
                    "scope": {"type": "string", "enum": ["channel", "project", "workspace", "member"]},
                    "mode": {"type": ["string", "null"], "enum": ["ask-all", "accept-edits", "yolo", null]}
                }}),
            ),
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
        "run_state": r.run_state.as_deref(),
        "command": r.command.as_deref(),
        "run_exit_code": r.run_exit_code,
        "run_output": r.run_output.as_deref(),
        "run_finished_at": r.run_finished_at,
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
        "chat_approval_mode_status" => {
            let channel = scope.channel;
            let member_mode = engine.approval_mode(scope.agent);
            let channel_mode = crate::store::channel_approval_mode(engine.conn(), channel)?
                .map(|m| ApprovalMode::parse(&m).as_str().to_string());
            let project_mode = match crate::store::channel(engine.conn(), channel)? {
                Some(ch) => {
                    crate::store::project_setting(engine.conn(), &ch.project, "approval_mode")?
                        .map(|m| ApprovalMode::parse(&m).as_str().to_string())
                }
                None => None,
            };
            let workspace_mode = crate::store::app_setting(engine.conn(), "approval_mode")?
                .map(|m| ApprovalMode::parse(&m).as_str().to_string());
            let effective = engine.effective_approval_mode(scope.agent, channel);
            Ok(json!({
                "effective": effective.as_str(),
                "member_default": member_mode.as_str(),
                "workspace_override": workspace_mode,
                "project_override": project_mode,
                "channel_override": channel_mode,
                "note": "Effective mode applies to new requests only: channel > project > workspace > member."
            }))
        }
        "chat_set_approval_mode" => {
            // Policy changes are human-authorized. Agents must not be able to
            // elevate their own effective permissions or change broader scopes.
            Err(ToolError(
                "only a human may change approval policy (use the app settings)".into(),
            ))
        }
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
                RequestKind::Other,
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

    #[test]
    fn mode_verdict_covers_all_kinds() {
        use super::{ModeOutcome, RequestKind, mode_verdict};
        use crate::types::ApprovalMode;
        for kind in [
            RequestKind::Read,
            RequestKind::Edit,
            RequestKind::Delete,
            RequestKind::Move,
            RequestKind::Execute,
            RequestKind::Fetch,
            RequestKind::Other,
        ] {
            assert_eq!(
                mode_verdict(ApprovalMode::AskAll, kind),
                ModeOutcome::AskHuman
            );
        }

        // STRM-596/606: reads/local search, generic structured edits,
        // delete/move, fetch, and other/mixed/unclassifiable operations require
        // human approval in both ask-all and accept-edits on the mediated path
        // (edits/delete/move fail closed until verified prerequisites exist).
        for kind in [
            RequestKind::Read,
            RequestKind::Edit,
            RequestKind::Delete,
            RequestKind::Move,
            RequestKind::Fetch,
            RequestKind::Other,
        ] {
            assert_eq!(
                mode_verdict(ApprovalMode::AcceptEdits, kind),
                ModeOutcome::AskHuman
            );
        }
        assert_eq!(
            mode_verdict(ApprovalMode::AcceptEdits, RequestKind::Execute),
            ModeOutcome::Queue
        );

        for k in [
            RequestKind::Read,
            RequestKind::Edit,
            RequestKind::Delete,
            RequestKind::Move,
            RequestKind::Execute,
            RequestKind::Fetch,
            RequestKind::Other,
        ] {
            assert_eq!(mode_verdict(ApprovalMode::Yolo, k), ModeOutcome::Allow);
        }
    }

    #[test]
    fn request_kind_from_wire_distinguishes_delete_and_move() {
        assert_eq!(request_kind_from_wire(Some("edit")), RequestKind::Edit);
        assert_eq!(request_kind_from_wire(Some("delete")), RequestKind::Delete);
        assert_eq!(request_kind_from_wire(Some("move")), RequestKind::Move);
        assert_eq!(
            request_kind_from_wire(Some("switch_mode")),
            RequestKind::Other
        );
        assert_eq!(request_kind_from_wire(Some("think")), RequestKind::Other);
        assert_eq!(request_kind_from_wire(Some("unknown")), RequestKind::Other);
        assert_eq!(request_kind_from_wire(Some("  edit  ")), RequestKind::Edit);
        assert_eq!(request_kind_from_wire(Some("")), RequestKind::Other);
        assert_eq!(request_kind_from_wire(Some("   ")), RequestKind::Other);
        assert_eq!(request_kind_from_wire(None), RequestKind::Other);
    }

    #[test]
    fn cc_control_plane_auto_allow_is_exact_and_scoped() {
        for allowed in [
            "chat_send",
            "chat_react",
            "chat_status",
            "chat_refuse",
            "chat_dissent",
            "chat_request_permission",
        ] {
            assert!(is_auto_allowed_cc_control_plane_tool(allowed), "{allowed}");
        }
        for denied in [
            "chat_context",
            "chat_read",
            "chat_search",
            "chat_permission_status",
            "chat_approval_mode_status",
            "chat_answer_permission",
            "chat_set_approval_mode",
            "chat_unknown_control_plane_tool",
            "CHAT_SEND",
            " chat_send",
            "chat_send ",
            " chat_send ",
        ] {
            assert!(
                !is_auto_allowed_cc_control_plane_tool(denied),
                "{denied} must not be auto-allowed"
            );
        }
    }

    #[test]
    fn structured_edit_prerequisites_are_unavailable_fail_closed() {
        assert!(
            !structured_edit_prerequisites_verified(),
            "core must not claim verified editable-root/execution-time evidence"
        );
        assert!(!execute_auto_allow_eligible_without_yolo());
        assert!(!StructuredEditEvidence::Unverified.allows_auto_edit());
        // Evidence-gated verdict: only a verified pure structured edit may be
        // auto-allow-eligible in accept-edits; delete/move/others never.
        assert_eq!(
            mode_verdict_with_edit_evidence(
                ApprovalMode::AcceptEdits,
                RequestKind::Edit,
                StructuredEditEvidence::Unverified
            ),
            ModeOutcome::AskHuman
        );
        assert_eq!(
            mode_verdict_with_edit_evidence(
                ApprovalMode::AcceptEdits,
                RequestKind::Edit,
                StructuredEditEvidence::_Verified
            ),
            ModeOutcome::Allow
        );
        for kind in [
            RequestKind::Delete,
            RequestKind::Move,
            RequestKind::Other,
            RequestKind::Read,
            RequestKind::Fetch,
            RequestKind::Execute,
        ] {
            assert_eq!(
                mode_verdict_with_edit_evidence(
                    ApprovalMode::AcceptEdits,
                    kind,
                    StructuredEditEvidence::_Verified
                ),
                if kind == RequestKind::Execute {
                    ModeOutcome::Queue
                } else {
                    ModeOutcome::AskHuman
                },
                "non-edit kinds must never become auto-allow-eligible edits: {kind:?}"
            );
        }
    }

    #[test]
    fn effective_request_kind_downgrades_conflicting_edit_payloads() {
        assert_eq!(
            effective_request_kind(RequestKind::Read, Some(r#"{"kind":"delete"}"#)),
            RequestKind::Read
        );
        assert_eq!(
            effective_request_kind(RequestKind::Edit, None),
            RequestKind::Edit
        );
        assert_eq!(
            effective_request_kind(RequestKind::Edit, Some("not json")),
            RequestKind::Edit
        );
        assert_eq!(
            effective_request_kind(
                RequestKind::Edit,
                Some(r#"{"subject":{"toolCall":{"kind":"delete"}}}"#)
            ),
            RequestKind::Other
        );
        assert_eq!(
            effective_request_kind(
                RequestKind::Edit,
                Some(r#"{"subject":{"toolCall":{"rawInput":{"nested":{"kind":"move"}}}}}"#)
            ),
            RequestKind::Other
        );
        assert_eq!(
            effective_request_kind(
                RequestKind::Edit,
                Some(r#"{"subject":{"toolCall":{"rawInput":{"ops":[{"kind":"execute"}]}}}}"#)
            ),
            RequestKind::Other
        );
        assert_eq!(
            effective_request_kind(
                RequestKind::Edit,
                Some(r#"{"subject":{"toolCall":{"rawInput":{"ops":[{"kind":"fetch"}]}}}}"#)
            ),
            RequestKind::Other
        );
        assert_eq!(
            effective_request_kind(
                RequestKind::Edit,
                Some(r#"{"subject":{"toolCall":{"rawInput":{"kind":"switch_mode"}}}}"#)
            ),
            RequestKind::Other
        );
        assert_eq!(
            effective_request_kind(
                RequestKind::Edit,
                Some(r#"{"subject":{"toolCall":{"kind":"edit","rawInput":{"path":"a.rs"}}}}"#)
            ),
            RequestKind::Edit
        );
        // Generic/ambiguous kind markers in arbitrary payload data are not, by
        // themselves, treated as verifiable conflicting operation-kind signals.
        assert_eq!(
            effective_request_kind(
                RequestKind::Edit,
                Some(r#"{"subject":{"toolCall":{"kind":"edit","rawInput":{"kind":"other"}}}}"#)
            ),
            RequestKind::Edit
        );
        assert_eq!(
            effective_request_kind(
                RequestKind::Edit,
                Some(r#"{"subject":{"toolCall":{"kind":"edit","rawInput":{"kind":"unknown"}}}}"#)
            ),
            RequestKind::Edit
        );
    }
}
