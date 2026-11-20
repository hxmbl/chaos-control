# STRM-612: OpenCode V2 Native Integration — Workstreams, Contract, and Classification

**Status:** In progress. Transport/E2E baseline exists in draft [PR #80](https://github.com/hxmbl/chaos-control/pull/80) (merge-ready, draft); permission mediation/verification is the roadblock.
**Tracking:** all workstreams tracked under [STRM-612](https://linear.app/storm-of-thoughts/issue/STRM-612/opencode-v2-native-integration-and-execution-architecture) (STRM-606/607/610 are canceled; do not use as live owners).

## Agreed split (chat)

- **Slice 0 (agreed):** treat PR #80 as transport-only with the permission gap explicitly labeled unverified; do not mix permission semantics into that merge.
- **@opencode-big-pickle (adapter + spike):** run a real `opencode serve` verification spike (which ops emit `permission.asked`, honored reply decisions/option IDs including deny/decline, and ops with no prompt), then replace `crates/core/src/opencode_v2.rs:295-319` auto-allow with real permission surfacing/reply plumbing (passthrough provider option IDs, genuine decline, hold for decision, fail-closed on stream/callback loss) wired to CC’s approval interface.
- **@opencode-iceman-free (contract + classification):** define CC-owned approval request model/correlation, policy gating for `ask-all`/`accept-edits` only when verified (else unsupported/unverified), and produce/update evidence-based operation classification plus retained ACP-guarantees doc.
- **Shared (fixtures/tests, with @devin welcome):** replay fixtures through the real orchestrator for the STRM-612 AC matrix (allowed, denied, bypass/no-prompt-unmediated, duplicate/replay, failure, disconnect/interruption, callback-loss/recovery), then document guarantees/classification evidence and tick ACs.

**Ordering:** spike is independent and informs evidence; adapter plumbing needs the CC approval interface shape pinned first (below), then refined with spike findings. Initial spike completed by @opencode-big-pickle (#248); sections below incorporate those results.

## Pinned minimal approval interface (refined with spike evidence)

Goal: Chaos Control owns approval/request identity and state; the adapter preserves provider request refs/semantics and never synthesizes provider options.

- **Adapter → CC: `request_approval(req)` (awaitable; blocks provider reply until resolved/canceled/fail-closed)**
  - `cc_request_id`: CC-generated, stable/unique per request.
  - `provider`: e.g. `"opencode_v2"`; `provider_request_ref`: opaque ref/id from `permission.asked` (required to reply; v2: `data.id`).
  - `session_id` / `execution_id`: CC execution node/session correlation.
  - `operation`: `{ kind, tool_name?, summary? }` and `arguments`/input (exact, as received). For v2, `permission.asked` gives `action`/`resources`/`save`/`source`; exact tool args may arrive in separate `session.tool.input.ended`/`session.tool.called` events keyed by `source.id` (tool-call UUID) — adapter must correlate and attach args when available.
  - `options: ProviderOption[]`: passthrough, provider-supplied, ordered — `{ id, label?, kind/semantics?, raw }`. **v2 spike finding:** `permission.asked` carries **no options array/IDs**; v2 uses a fixed decision vocabulary instead (`reject|once|always`, evidence below). In that case adapter exposes a provider-derived decision vocabulary (not invented UI-only options) and CC selects among those verified choices.
  - `requested_at`, optional `context` (e.g. subagent/child origin if exposed; v2 includes `source`).

- **CC → Adapter: `ApprovalDecision`**
  - `cc_request_id`.
  - `selected_option_id` or `selected_decision`: must be one of the provider-honored choices for this request, passed back in the provider’s expected form (v2: `{"decision":"reject"|"once"|"always"}`); **or** `resolution: "lost" | "timeout" | "disconnect" | "cancelled"` meaning no provider reply (fail closed).
  - `decided_at`, optional `actor`.

- **Adapter → CC: `record_outcome(outcome)` (and/or resolve awaitable)**
  - `cc_request_id`.
  - `state: "sent" | "denied" | "allowed" | "failed" | "not_sent"`.
  - `provider_reply?` (raw), `execution_effect?` (ran/blocked/error when knowable), `error?`.
  - **Invariant:** on `lost|timeout|disconnect|cancelled|ambiguous` → `not_sent`/`failed` as fail-closed, never treated as approval, never replayed.

**Rules:** one decision per `cc_request_id` (idempotent/replay-safe), preserve provider-honored decision semantics exactly, correlate decision↔outcome to the same CC request, and surface unsupported/unverified when callback state is missing. v2 server enforces one-shot replies (second reply → 404 `PermissionNotFoundError`), aligning with no-replay. If spike shows a different reply wire shape, adapter maps from CC’s chosen verified decision but CC must not guess unsupported semantics.

## Implementation evidence

### OpenCode v2 adapter (`crates/core/src/opencode_v2.rs`)
- **Session/transport:** native `opencode serve` HTTP/SSE (`open_session`, `stream_events`), session create, prompt delivery, interrupt, MCP injection via `/api/experimental/mcp/chaos-control`.
- **Pre-change baseline:** the earlier `answer_permission` path unconditionally replied `{"decision":"once"}` for `permission.asked` events, with no CC correlation, user decision, deny path, or fail-closed handling.
- **Current slice:** permission events now create CC-owned approval requests with provider request refs, fixed v2 decisions (`reject|once|always`), runtime broker correlation, explicit outcomes, and no provider reply on broker/loss/error paths. The runtime broker emits approval requests and validates selected decisions before resolving the provider reply.
- **Event mapping:** `EventMapper` handles `session.text.delta`, `session.text.ended`, `session.execution.succeeded|interrupted|failed`; approval payloads are correlated separately so provider decisions do not stall the SSE event loop.

### Spike evidence: `opencode serve` v2.3.2 (Big Pickle, #248)

Test setup: real `opencode serve` v2.3.2 over loopback API + SSE capture, `{"permission":{"*":"ask"}}`, model `opencode/big-pickle`.

- **`permission.asked` payload:** `{ id, sessionID, action, resources, save, source: { type: "tool", messageID, id } }` — **no options array and no provider option IDs**. `action` examples include `shell`/`read`; `resources` holds raw target (e.g. `["echo SPIKE_ONCE"]`); `save` holds grant pattern (e.g. `["echo *"]`).
- **Tool-arg correlation:** exact tool arguments are in separate `session.tool.input.ended` / `session.tool.called` events keyed by `source.id` (tool-call UUID). CC/adapter must correlate permission requests to that tool-call id to capture exact operation/arguments.
- **Reply API:** `POST /api/session/{id}/permission/{request}/reply` with `{"decision": ...}`.
  - `reject` → 204 + `permission.replied { reply: "reject" }` + `session.tool.failed { "type":"aborted", "message":"The user declined this tool call" }` with `executed:false` (**real deny, tool did not run**).
  - `once` → 204, tool then ran (`session.tool.success`, output observed, exit 0).
  - `always` → 204 and a follow-up read did **not** re-ask (grant saved; `save` pattern relevant).
  - invalid/missing decision → 400 `Expected Permission.Reply at ["decision"]`.
  - replying twice to same request → 404 `PermissionNotFoundError` (server enforces one-shot; aligns with CC idempotency/no-replay).
- **Implication:** v2 approval UI/contract must be decision-vocabulary-based (`reject|once|always` as verified), not option-ID-based; CC must still own request identity/correlation/state and fail closed on loss.

### ACP v1/v2 adapters (current, to retain where applicable)
- **ACP v1 (`crates/core/src/acp.rs`):** `v1_answer` selects `auto_allow` over offered options and replies `Selected(option_id)` or `Cancelled` if none (`auto_allow` prefers allow-once, then allow-always, else first). Request handled inline in `on_receive_request` for `session/request_permission`.
- **ACP v2 draft (`crates/core/src/acp_v2.rs`):** `answer` uses same `auto_allow`/option-id passthrough behavior over v2 permission options, replies `Selected` or `Cancelled`.
- **`auto_allow` (`crates/core/src/adapter.rs`):** prefers `AllowOnce` > `AllowAlways` > first option > `None` (cancelled). This is current auto-allow policy, not CC-mediated approval.
- **Docs (`docs/agents.md`, `crates/core/src/lib.rs`):** permissions currently described as not mediated/being redesigned; native ACP permission requests answered at once with agent’s own allow option.

## Retained ACP guarantees (current behavior to preserve/document explicitly)

These describe **today’s** behavior unless/until a scoped change is made; STRM-612 must not silently break them.

| Area | Current guarantee/behavior | Evidence |
|------|-----------------------------|----------|
| ACP transport/session flow | Existing ACP v1/v2 paths remain available as separate protocol adapters (v2 draft tried/falls back as implemented). | `crates/core/src/acp.rs`, `acp_v2.rs` |
| Permission request reply shape | ACP adapters reply with provider-offered option IDs (`Selected(option_id)`) or `Cancelled` when no option is selectable. | `acp.rs:678-686`, `acp_v2.rs:123-131` |
| Auto-allow choice (current) | Chooses allow-once, else allow-always, else first offered option. | `adapter.rs:auto_allow` |
| No user approval mediation (current) | Permission requests are answered immediately in-adapter; no prompt/queue/log surfaced by these adapters. | `docs/agents.md`, `lib.rs`, adapter code |
| Chat tools exposure | Injected per negotiated transport/capabilities; not gated by CC permission mediation today. | `acp.rs`, `acp_v2.rs`, `opencode_v2.rs` |
| Trust boundary claim | Current auto-answering is **not** a complete security boundary; agents may have unmediated paths/native controls. | `docs/agents.md`, STRM-612 issue text |

**To document as part of completion:** which ACP paths remain in product, their actual enforcement level, and any changes required for new permission semantics (must be explicit, not silent).

## Operation classification (evidence-based, update as evidence grows)

Classification buckets: **CC-mediated** (CC owns request/decision/outcome and enforces before side effects in verified boundary), **Provider-native** (provider enforces; CC may observe but not control), **Unverified/Unsupported** (unknown or cannot be guaranteed).

| Operation / event | Emits permission request? | Provider decision semantics honored? | Enforcement boundary | Classification | Evidence | Notes |
|---|---:|---:|---|---|---|---|
| v2 `permission.asked` (transport/request shape) | Yes (observed) | `reject`/`once`/`always` honored; invalid/missing → 400; duplicate reply → 404 | Provider can deny before tool execution (`reject` → `executed:false`, aborted) | Request mechanism **verified**; runtime CC mediation **implemented** | spike #248, `opencode_v2.rs:295-319` | No option IDs; correlate via `source.id` to tool-call events for exact args |
| v2 shell/exec (`action:"shell"`, e.g. `echo`) | Yes (observed in spike) | `reject` blocks; `once` allows; `always` grants pattern (`save`) | Provider-side gate before execution (per observed deny) | Provider-gated with **CC-owned request/reply mediation** | spike #248 | Need broader command coverage/evidence |
| v2 file read (`action:"read"` in follow-up) | Not re-asked after `always` in spike case | `always` grant applied (pattern-saved) | Provider-native grant behavior observed | **Partially evidenced / provider-native grant behavior** | spike #248 | Baseline "asks on read?" under `*":"ask"` still needs explicit first-hit evidence per op |
| v2 file edit/write/delete | TBD | TBD | TBD | Unverified | spike pending | Critical for `accept-edits` eligibility; capture per-op evidence |
| v2 network/fetch/web ops | TBD | TBD | TBD | Unverified | spike pending |  |
| v2 MCP tool calls (incl. chaos-control tools) | TBD | TBD | TBD | Unverified | `opencode_v2.rs:592-612`, spike pending | Distinguish MCP server install from per-call permission mediation |
| v2 operations with **no prompt** | n/a | n/a | none observed by request path | **Bypass risk / Unverified** until disproven | spike pending | Must be reported, not treated as protected |
| ACP v1 `session/request_permission` | Yes | Yes (option IDs passed; cancelled possible) | adapter auto-allows today (not CC-mediated) | Provider request observed, **currently auto-allowed** | `acp.rs:764-771`, `678-686` | Retained behavior; future mediation changes must be explicit |
| ACP v2 `RequestPermissionRequest` | Yes | Yes (option IDs passed; cancelled possible) | adapter auto-allows today (not CC-mediated) | Provider request observed, **currently auto-allowed** | `acp_v2.rs:322-329`, `123-131` | Same |

## Spike capture checklist (Big Pickle)

1. **Request emission:** for each op category (read, edit/write/delete, exec, network, MCP/tool, subagent/child if applicable) record whether `permission.asked` fires; capture raw event payload fields (`id/request ref`, `sessionID`, op/tool identifiers, args/input, options, any child/subagent linkage).
2. **Reply semantics:** which decisions/option IDs are honored (`once`, `always`, deny/reject, custom IDs), exact reply endpoint/body shape, behavior on invalid/unknown option, and whether deny truly prevents execution.
3. **No-prompt/unmediated paths:** operations that execute without `permission.asked`; classify as unsupported/unverified for CC protection with repro evidence.
4. **Failure/loss cases:** stream disconnect mid-request, server/session gone, callback/response after timeout or duplicate reply, restart/recovery behavior.
5. **ACP parity check:** confirm retained ACP paths still behave as documented after any v2 work.

### Spike findings recorded (#248, v2.3.2)
- [x] Basic request payload/ids/source captured (`permission.asked` has no options array).
- [x] Reply decisions verified: `reject` (deny, no execution), `once`, `always` (grant saved), invalid/missing → 400, duplicate reply → 404.
- [x] Tool-arg correlation path identified (`session.tool.input.ended`/`session.tool.called` keyed by `source.id`).
- [ ] Coverage across op categories (edit/write/delete, network, MCP tool calls, subagent/child) and explicit first-hit ask behavior per op.
- [ ] No-prompt/unmediated operation inventory.
- [ ] Disconnect/callback-loss/recovery behavior under real failure conditions.
- [ ] ACP parity confirmation post-change.

### T3 Code reference review

The T3 Code reference material was reviewed on 2026-11-19:

- **Provider boundary:** provider-specific behavior stays in the provider adapter; orchestration reads capabilities rather than guessing.
- **Permission modes:** offer only modes the provider enforces natively. Do not imitate a missing mode by automatically answering approval callbacks.
- **Approval semantics:** pass provider-owned option IDs through unchanged and require an honored decline path.
- **Lifecycle:** replay provider transcripts through the real orchestrator/adapter/projections; test interruption, failure, recovery and terminal pending-request settlement.
- **ACP:** ACP agents run their own tools and sandbox/approval controls; ACP approval handling does not prove Chaos Control controls every operation.
- **Resumption:** lost runtime state may preserve durable provider/session references, but it must not treat a lost callback or ambiguous response as an approved or resumable operation.

This review supports the current STRM-612 design and identifies replayed real-provider transcripts through the real orchestrator, plus durable recovery of pending requests, as the remaining high-value evidence.

## Fixture/test matrix (STRM-612 AC coverage target)

| Category | Expected behavior | Notes |
|---|---|---|
| Allowed | explicit user approval of a covered request allows exactly that operation once (as scoped) | preserve provider option id; correlate CC request/outcome |
| Denied | explicit deny prevents execution | must use provider-honored deny/decline path |
| Bypass / no-prompt-unmediated | operation executes or is attempted without request | must be detected and reported unverified/unsupported, not claimed protected |
| Duplicate / replay | repeated/duplicate request or reply is idempotent | no double execution, no replayed approval side effects |
| Failure | provider/operation fails before/after decision | outcome recorded accurately; no false approval state |
| Disconnect / interruption | transport/session interruption during pending or executing request | fail closed; no silent approval; lifecycle handled per issue scope |
| Callback loss / recovery | response callback/session state lost or resumed ambiguously | fail as not resumable when needed; never replay/approve implicitly |

**Pending-request policy:** in-flight approval prompts are runtime-only state. If
Chaos Control restarts or loses the prompt callback, the provider request is
settled as `Cancelled`/`Lost`/`Disconnect` and recorded `not_sent`; it is never
rehydrated as approved or replayed. Persisting prompt payloads is optional future
work, not required for fail-closed crash recovery.

**Request model:** mediation is per operation, not a queue or retry grant. Each
request has one CC identity and receives one explicit resolution; duplicate IDs
are rejected, and a resolution is not reused for later operations. Harnesses
that bypass provider permission callbacks remain a gap: they must be inventoried
and labeled unprotected/unverified rather than inferred to be mediated.

**Mode-gating decision (2026-11-20):** `ask-all` and `accept-edits` are classified
**Unsupported/unverified** for OpenCode V2. No real-V2 evidence yet shows either
mode's effective behavior is enforced, so neither is offered as an enforced mode,
and no provider callback is auto-answered to imitate one. The per-operation
approval path above (CC-owned IDs, `reject|once|always`, one-shot, fail-closed) is
unchanged and still mediated. This is a scope label, not a claim that the modes
work; it is recorded on STRM-612 and re-openable with real-flow evidence.

**Failure-visibility decision (2026-11-20):** unsupported and unverified operations
or modes must **fail loudly**, not silently — surface the blocked/unsupported state
in the right sidebar so the user can see what was refused or is not covered. The
heavy real-orchestrator/T3 replay pass is **deferred, not dropped**; near-term we
prefer this visible fail-closed behavior and incremental doc updates over building
the full replay matrix now.

**Harness note:** fake agent (`crates/core/src/fake.rs`) already supports permission request steps and records outcomes; v2 adapter/orchestrator wiring will need real CC approval plumbing to drive these end-to-end.

## Evidence ledger scaffold (foundation only — not yet evidence)

Every row stays `TODO` until it is exercised through the real orchestrator/V2 adapter with a linked artifact. Broker unit tests do not tick acceptance criteria.

| Matrix row | AC | Existing unit-level evidence | Status |
|---|---|---|---|
| Allowed (one-shot) | 4, 9 | `runtime_approval_broker_emits_validates_and_records_decisions` | TODO: real V2 op |
| Denied | 4, 9 | spike: `reject` → `executed:false` | TODO: real V2 op |
| Bypass / no-prompt-unmediated | 7, 8, 9 | inventory stub below | TODO |
| Duplicate / replay | 6, 9 | `1dc147a` duplicate-pending rejection | TODO: real orchestrator |
| Failure | 9 | — | TODO |
| Disconnect / interruption | 9, 10 | `46eb91e` stream-close settlement | TODO: real V2 stream |
| Callback loss | 10 | `b2edc26` lost-callback fail-closed | TODO: real V2 session |
| Mode gating (`ask-all`/`accept-edits`) | 5 | — | Unsupported/unverified by scope decision (2026-11-20); no simulated modes |
| ACP retained paths | 2 | — | TODO: regression check + one-line doc |

**Per-op inventory stub** (fill from a real V2 session; `?` means unknown — never assume mediated):

| Op | Emits request? | Provider-enforced before exec? | CC enforcement? | Evidence |
|---|---|---|---|---|
| shell/execute | ? | ? | ? | — |
| read | ? | ? | ? | — |
| edit/write/delete | ? | ? | ? | — |
| network/fetch | ? | ? | ? | — |
| MCP tool call | ? | ? | ? | — |
| subagent/child | ? | ? | ? | — |
| no-prompt paths | n/a | ? | unsupported/unverified until proven | — |

## Next actions / pickup checklist (foundation)

### Already evidenced (landed)
- [x] CC-owned request/decision/outcome contract + broker correlation, provider decision semantics preserved, fail-closed unresolved replies (`crates/core/src/approval.rs`, `crates/core/src/opencode_v2.rs`, `crates/core/tests/opencode_v2_approval.rs`).
- [x] Broker unit coverage: emit/validate/record decisions, duplicate pending rejected + one-shot cancel, lost event callback fail-closed, cancel-pending resolves as disconnect (`crates/core/src/host.rs`, `1dc147a`, `b2edc26`).
- [x] Stream disconnect settles unresolved pending approvals as `Disconnect`/`not_sent` (`46eb91e`).
- [x] Durable outcomes/persistence baseline recorded (`3076c95` per sync); per-request model + fail-closed pending policy documented (`b22db97`, this doc).

### Remaining evidence to collect (real orchestrator / real v2 flow where possible)

**Priority note (2026-11-20):** the heavy real-orchestrator/T3 replay pass is deferred.
The currently running native V2 session is live evidence for the native transport/session
work (AC 1), but not for permission mediation unless a mediated decision is actually
exercised. Near-term focus: visible fail-closed behavior in the right sidebar plus
incremental doc updates; keep the items below open rather than marking them done.
- [ ] **Per-op mediation inventory:** for each op category record emits-request? honored `reject|once|always`? execution effect? exact args/correlation?
  - [ ] shell/exec (broaden beyond spike)
  - [ ] file read
  - [ ] file edit/write/delete (critical for `accept-edits`)
  - [ ] network/fetch/web
  - [ ] MCP tool calls (incl. chaos-control tools) — distinguish install vs per-call mediation
  - [ ] subagent/child tool calls (if exposed)
- [ ] **No-prompt/unmediated paths:** operations that run without `permission.asked` → inventory with repro, mark unsupported/unverified (not protected). Include harnesses that bypass provider permission callbacks.
- [ ] **Replay/AC matrix through real orchestrator** (extend `crates/core/tests/opencode_v2_approval.rs`, `crates/core/tests/v2.rs`, fixtures under `crates/core/tests/fixtures/` as needed):
  - [ ] allowed (covered request, once-scoped)
  - [ ] denied (provider-honored reject prevents execution)
  - [ ] bypass/no-prompt-unmediated detected + reported
  - [ ] duplicate/replay idempotent (no double exec/replayed approval)
  - [ ] failure (before/after decision) recorded accurately
  - [ ] disconnect/interruption mid-pending or mid-exec → fail closed
  - [ ] callback loss/recovery under real failure conditions → not resumable as approval, never replay
- [x] **Policy modes (scope decision 2026-11-20):** `ask-all` and `accept-edits` explicitly labeled **unsupported/unverified** for OpenCode V2; no simulated modes, no provider callbacks auto-answered to imitate a mode. Re-open with real-V2 evidence if enforcement is verified. Recorded on STRM-612.
- [ ] **AC 2 (keep in STRM-612):** quick ACP regression check of retained paths + one-line/short doc of actual guarantees (no ACP build-out).

### Close-out
- [ ] Update operation classification table + retained ACP guarantees with evidence links/commits.
- [ ] Tick STRM-612 ACs only where verified; note unsupported/unverified explicitly.
- [ ] Add PR #80 note summarizing permission/evidence status.

**Notes:** pending prompts remain runtime-only/fail-closed (no persistence/replay). Avoid colliding with in-progress broker/adapter edits in `crates/core/src/host.rs`, `crates/core/src/opencode_v2.rs`; prefer adding fixtures/tests/docs.
