# Command approval and execution modes (STRM-542)

> Status: foundation landed — `ApprovalMode` + scope resolution,
> `RequestKind` classification, `decided_by`, pending-as-live requests with
> sidebar Approve/Deny, and the queued-command fields (`command`,
> `run_state`, ...) are in. Not yet: core running queued commands and
> steering results back, `ApprovalCaps`/`decide` are still unused
> scaffolding, and the "Open decisions" below are partly settled by what
> shipped (universal decline+grant path; per-member `agents.json` mode with
> channel/project/workspace overrides).

How Chaos Control decides what an agent may do on the host, and what happens
to the things it wants to do. This extends the permission system in
`docs/agents.md` ("Permissions: by where the agent runs"); it does not
replace it. Reads stay free, VM agents stay auto-approved, and a declined or
denied request is still an answer, not a failure.

Three ideas carry the design:

- **An execution mode** per agent (or per channel, later): `ask-all`,
  `accept-edits`, `yolo`. It decides *whether* a request reaches a human and
  *what approving it means*.
- **A live queue** of pending requests the app can actually answer, instead
  of today's fire-and-forget log.
- **A capability contract** per adapter: what the protocol can observe,
  hold, or defer. Where a capability is missing we fail closed and say so —
  never silently allow.

## Execution modes

```rust
/// How much of an agent's host-side work a human signs off on.
/// Stored per member (agents.json `"approval"`, default `AskAll`);
/// a channel-scoped override is a later addition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Every non-read request waits for a human. Today's behavior,
    /// minus the immediate decline: the request stays live.
    AskAll,
    /// Edits and reads go through; commands queue for later execution
    /// and their output is steered back to the agent.
    AcceptEdits,
    /// Nothing asks. Recorded for the log as auto-approved-by-mode,
    /// distinct from a VM's auto-approval so the log can tell them apart.
    Yolo,
}
```

```rust
/// What a mode does with one incoming request, before any human sees it.
/// Pure function of (mode, request kind) — no I/O, fully testable.
fn mode_verdict(mode: ApprovalMode, kind: RequestKind) -> ModeOutcome {
    match mode {
        // Reads never reach this function: is_read_only_kind short-circuits
        // upstream, as today.
        ApprovalMode::AskAll => ModeOutcome::AskHuman,
        ApprovalMode::AcceptEdits => match kind {
            RequestKind::Edit | RequestKind::Read => ModeOutcome::Allow,
            RequestKind::Execute | RequestKind::Fetch | RequestKind::Other => {
                ModeOutcome::Queue
            }
        },
        ApprovalMode::Yolo => ModeOutcome::Allow,
    }
}
```

Mode changes are not retroactive: switching to `yolo` does not approve
requests already pending, and switching to `ask-all` does not un-approve
queued commands. Every decision records *which* mode (or rule, or human)
made it — the permission log gains a `decided_by` column so an audit can
tell "auto-approved by yolo" from "approved by @hx".

## The live queue

Today `ask_permission` inserts a pending row, posts a note, and the adapter
has already been told "declined, retry later". The queue keeps the same
non-blocking shape — the agent is never held — but makes the pending set
first-class so the app can render it and answers can be delivered while the
agent is still in its turn.

```rust
/// One thing an agent wants to do that a human (or a mode) hasn't settled.
/// This is the row the sidebar renders and the app's Approve/Deny acts on.
/// Wraps/extends `permission_requests`; the store table is the same,
/// the queue is the *view* plus the delivery bookkeeping.
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
}

pub enum QueueState {
    /// Waiting for a human (or for its mode rule to apply).
    Pending,
    /// Approved; the command is running (accept-edits) or the grant is
    /// armed for the agent's retry (ask-all).
    Approved,
    /// Denied by a human, with the reason that will be steered back.
    Denied { reason: Option<Arc<str>> },
    /// Ran to completion; `result` is what gets delivered to the agent.
    Done { result: Option<Arc<str>> },
    /// Approved but execution failed (command exited non-zero, agent
    /// unreachable). The agent hears the failure, not silence.
    Failed { error: Arc<str> },
}
```

```rust
impl Engine {
    /// The queue for a channel, pending first then recent outcomes.
    /// This is what the sidebar's Permissions pane binds to — it replaces
    /// the read-only log as the primary surface; the log stays for history.
    pub fn approval_queue(&self, channel: ChannelId) -> Vec<QueuedRequest> {
        todo!()
    }

    /// A human approves or denies a queued request. For `ask-all` this is
    /// today's `answer_permission` (one-shot grant + wake). For
    /// `accept-edits` commands, approval also *runs the command* and the
    /// result — not just the grant — is steered back to the agent.
    pub fn resolve_queued(
        &mut self,
        now: Millis,
        id: PermissionId,
        by: MemberId,
        approve: bool,
        note: Option<&str>,
    ) -> Result<QueuedRequest, AnswerError> {
        todo!()
    }
}
```

Delivery back to the agent reuses the existing wake path: an approval
posts a system message and inserts a `WakeReason::Permission` trigger,
exactly as `answer_permission` does today. What changes is the *content* —
for a queued command the message carries the exit status and output tail,
so the agent doesn't have to retry-and-hope.

## Proposed changes (diffs)

A real diff only exists if the agent sends one. ACP tool calls can carry
`ToolCallContent::Diff`; whether a given harness populates it is a
capability, discovered at handshake, not assumed.

```rust
/// What the agent says it wants to change, when the protocol gives us
/// enough to show a human. `None` means "show the title/detail only" —
/// we never fabricate a diff from the command line.
pub enum ProposedChange {
    /// Old/new text for one file (ACP diff content).
    Text { path: Arc<str>, old: Arc<str>, new: Arc<str> },
    /// A unified diff blob, when that's all the agent sent.
    Unified { path: Arc<str>, patch: Arc<str> },
}

/// Attached to `PermissionAsk`/`QueuedRequest` when the adapter could
/// extract one. Drives whether the UI shows "approve this edit" with a
/// diff view or falls back to the plain request row.
pub diff: Option<ProposedChange>,
```

## The capability contract

Each adapter declares, per live session, what it can do with a permission
request. This is the honest boundary the whole feature stands on.

```rust
/// What one live session's protocol can actually do for us.
/// Sampled from the handshake/negotiation, stored on `SessionInfo`.
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
```

```rust
/// The decision the gate makes, given caps. Fail closed: a capability
/// we don't have means we decline-and-ask, never allow-and-hope.
fn decide(
    caps: ApprovalCaps,
    mode: ApprovalMode,
    kind: RequestKind,
) -> PermissionVerdict {
    match mode_verdict(mode, kind) {
        ModeOutcome::Allow => PermissionVerdict::Allow,
        ModeOutcome::AskHuman => {
            if caps.hold_request {
                // True blocking queue: keep the ACP call open, answer
                // when the human resolves. Agent waits — acceptable only
                // because the agent (or mode) opted into hold semantics.
                PermissionVerdict::Hold
            } else {
                // Universal path: decline now with "request sent #N",
                // grant on retry. Today's behavior, kept.
                PermissionVerdict::Sent { request: id }
            }
        }
        ModeOutcome::Queue => {
            if caps.deferred_execute {
                PermissionVerdict::DeferredQueue { request: id }
            } else {
                // Can't run it for them: queue the *ask*, agent retries
                // on approval like ask-all, output comes back as a wake.
                PermissionVerdict::Sent { request: id }
            }
        }
    }
}
```

Per-adapter notes (to verify live, not assume):

- **opencode / claude-agent-acp (ACP):** `hold_request: false` —
  `session/request_permission` is blocking but we answer at once by
  design. `proposed_diffs`: check whether tool calls carry diff content.
  `mode_switch`: claude-agent-acp has session mode switching; whether we
  should drive it is a separate question from our own `ApprovalMode`.
- **codex-acp:** exec approvals are command approvals with structured
  command text — the best candidate for real queue-then-execute
  (`QueuedRequest.command`). File changes arrive as `changes` payloads
  (already hashed for `request_key`). The native app-server adapter was
  removed in STRM-542; everything arrives over ACP now.

## Open decisions (need answers before slice 1)

1. **Queue semantics for `accept-edits`:** decline+grant-on-retry
   (universal, agent runs it) vs. hold-open (blocking, true queue) vs.
   deferred-execute (capability-gated). Recommendation: ship the
   universal path first, declare the contract so better adapters can
   upgrade later.
2. **Mode scope:** per-member in `agents.json` now; per-channel override
   deferred (no delegation model exists yet to inherit one).
3. **`yolo` naming:** it auto-approves *host* requests only. A VM agent
   is already auto-approved; `yolo` on a VM agent is a no-op. The log
   must distinguish `auto_approved` (VM) from mode-auto-approval.
4. **Diff display:** only when `proposed_diffs` is true *and* the agent
   actually sent one. Otherwise the row shows title/detail and the human
   approves blind — same as today, just live.
