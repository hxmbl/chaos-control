# Agent adapters: harnesses, detection and models

How Chaos Control finds the coding agents installed on a machine, starts
them, and picks the model each one runs. The room rules (wakes, delivery,
reply obligations, permissions) are in [`agents.md`](agents.md); this file
covers what sits under them.

> **Not live-verified.** Everything below is built and covered by tests
> against a scripted fake ACP agent and a fake Codex app-server
> (`crates/core/tests/models.rs`), but none of it has been run against a
> real harness yet. Hxmbl couldn't test when it landed. Treat every launch
> command and model detail here as theoretical until the manual checks at
> the end pass on a real machine.

## Members are models

A chat member is a **model** ("GPT-6.1 Sol", "Claude Opus 5.5", "Devstral")
running through a **harness** (Codex, fx, Mistral Vibe, Claude Code, ...).
The harness only decides how the process starts and which protocol it
speaks. The room never treats two members differently because of their
harness.

- One harness can host any number of members, one per model. Each member
  gets its own adapter, and so its own processes and sessions.
- `(harness, model)` is unique: the same model through the same harness,
  added to two projects, is one member granted to both.
- A member whose model is `None` runs the harness's own default model.

Code: `crates/core/src/harness.rs` (presets, detection, catalog, ranking),
`crates/core/src/roster.rs` (model members and rosters),
`crates/core/src/codex.rs` (native Codex adapter), `crates/core/src/acp.rs`
(ACP adapter, model option), `crates/app/src/agents.rs` (startup) and
`crates/app/src/model_picker.rs` (the picker).

## Harnesses

`harness::PRESETS`, in Hxmbl's priority order (2026-10-30). The ACP launch
commands come from the official ACP agent registry (checked 2026-10-30).
Pi was dropped.

| # | Id | Harness | Transport | Detected by | Launch | Model selection |
|---|---|---|---|---|---|---|
| 1 | `codex` | Codex | native Codex app-server | `codex` | `codex app-server` | `model/list`; `thread/start {model}` |
| 2 | `fx` | fx | ACP | `fx` | `fx acp` | ACP model config option |
| 3 | `vibe` | Mistral Vibe | ACP | `vibe-acp`, `vibe` | `vibe-acp` (sibling of `vibe`, else on the search path) | ACP model config option |
| 4 | `claude-code` | Claude Code | ACP via adapter | `claude`, `claude-agent-acp` | `claude-agent-acp`, else `npx -y @agentclientprotocol/claude-agent-acp@0.99.1` | ACP model config option |
| 5 | `copilot` | GitHub Copilot CLI | ACP | `copilot` | `copilot --acp` | ACP model config option |
| 6 | `opencode` | opencode | ACP | `opencode` | `opencode acp` | ACP model config option |
| 7 | `devin` | Devin CLI | ACP | `devin` | `devin acp` | ACP model config option |
| 8 | `cursor` | Cursor Agent | ACP | `cursor-agent` | `cursor-agent acp` | ACP model config option |
| 9 | `gemini` | Gemini CLI | ACP | `gemini` | `gemini --acp` | ACP model config option |

### Why these transports

- **Codex: native app-server.** ACP only reaches Codex through the
  `codex-acp` adapter, which lags the app-server and hides its model list.
  `codex app-server` is what Codex's own IDE extensions and T3 Code drive.
  `CodexAdapter` speaks its JSON-RPC (one object per line, no `"jsonrpc"`
  member) behind the same `AgentAdapter` / `AgentSession` boundary as ACP,
  so the runtime never knows the difference. Wire summary: `initialize` +
  `initialized`; `model/list` (paged); `thread/start {cwd, model?, config}`
  with our chat MCP server added as `mcp_servers.chaos-control`;
  `turn/start` per batch; `turn/interrupt` to cancel. Approval requests
  (`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`)
  go through the permission gate and are answered `accept` or `decline`.
  The module docs in `codex.rs` have the full method list and the
  acceptance point.
- **Everything else: ACP.** Each harness's own ACP entry point
  (`fx acp`, `vibe-acp`, `copilot --acp`, ...), all through the existing
  `AcpAdapter` (v2 draft offered first, v1 fallback; see `agents.md`).
- **Claude Code: adapter binary.** Claude Code doesn't speak ACP itself.
  The `claude-agent-acp` adapter is used when installed, else it runs
  through `npx` (pinned to 0.99.1), which needs Node. With neither
  `claude-agent-acp` nor `npx`, Claude Code counts as not installed.

Chaos Control never overrides a harness's approval, sandbox or mode
settings (fx's ask/code modes included). The only thing added to a session
is the chat MCP server.

## Detection

`harness::detect` checks, for each preset in order, its `detect` binaries:

1. every directory on `PATH`;
2. then the usual install directories (`harness::USUAL_DIRS`):
   `~/.local/bin`, `~/.fx/bin`, `~/.opencode/bin`, `~/.bun/bin`,
   `~/.npm-global/bin`, `~/.volta/bin`, `~/.cargo/bin`, `~/.cursor/bin`,
   `~/.devin/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`.

The extra directories matter on macOS: an app launched from Finder gets a
minimal `PATH` and would otherwise find nothing. The first executable match
wins (`.exe` is appended on Windows). The launch command is then resolved:
the detected binary itself, a sibling binary (`vibe-acp` next to `vibe`), a
lookup of the launch binary, or the adapter / `npx` path for Claude Code.

Detection only checks files. It never runs a harness. Turn it off with
`"detect": false` in `agents.json` or `CHAOS_CONTROL_DETECT=0`.

### When core starts

The app starts core when an `agents.json` exists **or** at least one harness
is detected. Starting core opens the launch folder's project, which writes
`.chaos/project.toml` there (see "Storage model" in `ARCHITECTURE.md`).
With neither, the app runs on sample data as before.

### Seeding a project's roster

The first time a project opens, every detected harness adds one member: its
first preset model (for Codex and Claude Code that is the harness default).
The project is then marked seeded (`roster_seeded`), and from then on only
the user changes the roster, in Agent Status. A member whose harness is no
longer installed stays on the roster and shows red.

An explicit `members` list in `agents.json` replaces seeding and is applied
at every start.

## Model discovery

Each preset carries a fallback model list, shown until the harness reports
its own:

- **Codex and Claude Code:** names from T3 Code's model manifest of
  2026-10-28 (plus "Default model").
- **Everyone else:** only "Default model" until the harness answers.

Discovery runs when the model picker opens (`ChatStore::refresh_models` ->
`Core::discover_models` -> `AgentAdapter::list_models`) for every harness
that has only its preset list. It spawns each such harness once:

- **ACP:** `initialize`, then a throwaway `session/new` with no chat tools,
  reading the session's model selector from `configOptions`: the `select`
  option whose category is `model`, else the one with id `model`. The
  session is closed straight away. Times out after 30 s.
- **Codex:** `initialize`, then every page of `model/list` (hidden models
  left out). No thread is started.

A non-empty answer is cached in `harness_models` and replaces the fallback.
A failure (signed out, timeout) caches nothing, and the picker's filter
entry for that harness says why.

### Selecting the model for a session

- **ACP:** after `session/new`, if the member has a model and the harness
  offers it in the model option, `session/set_config_option` sets it. If
  the harness doesn't offer it, the session runs the harness default and
  the member's presence says so ("running its default"). A member with no
  model sets nothing.
- **Codex:** `model` is passed on `thread/start` when `model/list` contains
  it; otherwise the thread starts on the default and presence says so.

## The model picker

Agent Status lists the project's members: model name, harness mark, status
dot, a remove button on hover, and an "Add a model" row that opens the
picker in the pane layer.

- The picker lists **models**, not harnesses. `harness::Catalog` groups the
  same model offered by several harnesses into one row.
- A search field on top, a harness filter row ("All" plus one chip per
  installed harness), then the ranked list. Up/Down move the highlight,
  Enter adds, Escape backs out of a step, then closes.
- A row one harness offers is added straight away. A row several harnesses
  can run opens a second step asking which one ("via X").
- A harness that already runs that model as a member is left out of the
  choice; when every harness offering it already does, the row does nothing.

### Credit: T3 Code (MIT)

[T3 Code](https://github.com/pingdotgg/t3code) is MIT licensed. Borrowed:

- **Picker structure** from `ModelPickerContent.tsx` and
  `ModelPickerSidebar.tsx`: search on top, harness filter, ranked list. T3
  draws the filter as a side rail; our narrower pane draws it as a row.
- **Search ranking** (`harness::rank`), ported from `modelPickerSearch.ts`.
- **Codex app-server method names and payload shapes**, from
  `CodexAdapterV2.ts` and `CodexProvider.ts`. Studied, not copied:
  `codex.rs` is a fresh Rust client.
- **Fallback model names** for Codex and Claude Code, from T3's model
  manifest of 2026-10-28.

## agents.json

Optional. Its full shape is in [`agents.md`](agents.md#running-it).

## Manual test commands (per harness)

None of these has been run yet. Each needs the harness installed and
signed in. Start with the harness alone, then through Chaos Control.

Protocol smoke test against the raw harness (ACP harnesses answer
`initialize` on stdin):

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}' | fx acp
```

Swap `fx acp` for each launch command:

| Harness | Command |
|---|---|
| fx | `fx acp` |
| Mistral Vibe | `vibe-acp` |
| Claude Code | `claude-agent-acp` or `npx -y @agentclientprotocol/claude-agent-acp@0.99.1` |
| Copilot CLI | `copilot --acp` |
| opencode | `opencode acp` |
| Devin CLI | `devin acp` |
| Cursor Agent | `cursor-agent acp` |
| Gemini CLI | `gemini --acp` |

Codex app-server (no `"jsonrpc"` member):

```sh
printf '%s\n' \
  '{"id":1,"method":"initialize","params":{"clientInfo":{"name":"chaos-control","version":"0.0.1"}}}' \
  '{"method":"initialized"}' \
  '{"id":2,"method":"model/list","params":{}}' | codex app-server
```

Live through core's ignored replay test (any ACP harness):

```sh
CHAOS_LIVE_ACP="fx acp" CHAOS_LIVE_RECORD=/tmp/fx.ndjson \
  cargo test -p chaos-core --test replay live -- --ignored --nocapture
```

In the app:

```sh
cargo run -p chaos-control                          # auto-detect
CHAOS_CONTROL_DETECT=0 cargo run -p chaos-control   # detection off
CHAOS_CONTROL_DATA=/tmp/cc-fresh cargo run -p chaos-control   # fresh store, re-seeds
```

What to check for each harness:

1. It shows up in Agent Status with its default model, and its dot goes
   green after `@handle hello`.
2. It answers in the room through `chat_send`.
3. "Add a model": its filter chip stops saying "usual models" once it lists
   its own, and picking one of those models adds a member that actually runs
   that model (ask it which model it is).
4. Removing the member takes it out of Agent Status; its old messages stay.
5. Signed out: the member goes red with one note and no retry loop.

## Tests

`cargo test -p chaos-core --test models` covers, against the fakes: an ACP
member setting its model through the session model option; a model the
harness doesn't offer running the default and saying so; a default member
setting nothing; ACP discovery opening and closing a throwaway session
without chat tools; a signed-out harness's discovery caching nothing; a
Codex member taking a turn with its last message as the fallback reply; a
Codex model missing from `model/list` starting on the default; Codex
approvals going through the permission gate; Codex signed out going red
without retries; Codex interrupt ending the turn cancelled; Codex listing
models without starting a thread; and one harness hosting two members.
Detection, catalog grouping and ranking have unit tests in `harness.rs`.
