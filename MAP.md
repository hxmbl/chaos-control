# MAP.md

A lookup table, not an architecture doc: where code lives and what to look for.
For *how* it works, follow the [`ARCHITECTURE.md`](ARCHITECTURE.md) links below.
For agents, harnesses and models, see [`docs/agents.md`](docs/agents.md) and
[`docs/agent-adapters.md`](docs/agent-adapters.md).

## Crates at a glance

| Crate | Role |
| --- | --- |
| `crates/app` | The `chaos-control` binary: GPUI window, root layout, views and state bridging to core. |
| `crates/core` | `chaos-core`, no GPUI: SQLite store, delivery engine, agent adapters, ACP/MCP, chat tools. |
| `crates/model` | Plain data types (`Id`, `Workspace`, `ChannelLog`, `PaneContent`, `Settings`), no UI deps. |
| `crates/ui` | Reusable GPUI components, theme tokens, icons, fonts and text input. |

Start at `crates/app/src/main.rs` (window creation) and
`crates/app/src/workspace.rs` (`Workspace`, root view).

## App: UI surfaces

| Surface | File | Key types / entry points |
| --- | --- | --- |
| Root layout and panels | `crates/app/src/workspace.rs`, `crates/app/src/panels.rs` | `Workspace`, `Side`, `Mode`, `plan`, `Panels` |
| Left sidebar (projects, channels, quick links) | `crates/app/src/sidebar.rs` | `Sidebar` |
| Right sidebar (mentions, blocked approvals, working-tree changes, agent status) | `crates/app/src/right_sidebar.rs` | `RightSidebar`, `ApprovalNotice`, `ChangedFile` rows |
| Conversation / message list | `crates/app/src/conversation.rs` | `Conversation`, `MessageMenu` |
| Composer (input, mentions, emoji, slash commands) | `crates/app/src/composer.rs` | `Composer`, `Draft`, `bindings` |
| Pane (long messages, approvals, project questions) | `crates/app/src/pane_host.rs` | `PaneHost`, `PaneEvent`, `pane_frame` |
| Pane content types and actions | `crates/model/src/pane.rs` | `PaneContent`, `PaneActionKind`, `ReplyRef` |
| Markdown rendering | `crates/app/src/markdown.rs` | `render_message_markdown` |
| Search | `crates/app/src/search.rs`, `crates/model/src/search.rs` | `Search`, `SearchResult`, `SearchProvider`, `SubstringSearch` |
| Settings | `crates/app/src/settings.rs`, `crates/model/src/lib.rs` | `AppSettings`, `Settings` |
| Model picker, mentions, profile, titlebar | `crates/app/src/{model_picker,mention_highlight,profile,titlebar}.rs` | `ModelPicker`, `KnownMentions`, `Profile`, `Titlebar` |
| Native window chrome | `crates/app/src/{dock,tray}.rs` | `init` |

Rendering model and perf context: [ARCHITECTURE.md#state](ARCHITECTURE.md#state),
[ARCHITECTURE.md#messages-render-as-markdown](ARCHITECTURE.md#messages-render-as-markdown),
[ARCHITECTURE.md#the-pane](ARCHITECTURE.md#the-pane),
[ARCHITECTURE.md#sidebars](ARCHITECTURE.md#sidebars).

## App: state and core wiring

| Concern | File | Key types / entry points |
| --- | --- | --- |
| App-side store and core event folding | `crates/app/src/store.rs` | `ChatStore`, `StoreEvent`, `Attached`, `pending_approval`, `apply_core_event` |
| Core attach / launch / harness detection | `crates/app/src/agents.rs` | `start`, `CoreLink`, `OpenProject`, `LinkedAgent`, `data_dir`, `agents_file` |
| `agents.json` edits | `crates/app/src/agents_file.rs` | `Entry`, `set_members`, `add_member`, `remove_member` |
| Project identity and `.chaos/project.toml` | `crates/app/src/project.rs`, `crates/app/src/project_context.rs` | `decide`, `Confirm`, `ProjectContext`, `ProjectLifecycle` |
| Recents and profile persistence | `crates/app/src/{recents,profile}.rs` | `RecentsFile`, `Profile` |
| Approval notices in the sidebar | `crates/app/src/right_sidebar.rs` | `ApprovalNotice::label` |
| Working-tree changes for the sidebar | `crates/app/src/store.rs` | `ChatStore::changed_files`, `changed_files_for`, `ensure_baseline`, `ChangedFile`, `ChatBaseline`, `parse_diff_stat` |

Core-side counterpart: [ARCHITECTURE.md#crates](ARCHITECTURE.md#crates),
[ARCHITECTURE.md#state](ARCHITECTURE.md#state).

## Core: storage and migrations

| Concern | File | Key types / entry points |
| --- | --- | --- |
| Split store (`core.db` + per-project DBs) | `crates/core/src/store.rs` | `Store::open`, `open_in_memory`, `ProjectDbs` |
| Schema and migrations | `crates/core/src/store.rs` | `CORE_MIGRATIONS`, `WORKSPACE_MIGRATIONS`, `AGENTS_MIGRATIONS`, `LEGACY_MIGRATIONS`, `run_migrations` |
| Row helpers (messages, cursors, batches, triggers, reactions, stances) | `crates/core/src/store.rs` | `insert_message`, `advance_read`, `dispatch_batch`, `insert_trigger`, `add_reaction`, `insert_stance` |
| Roster, ping answers, cached models | `crates/core/src/roster.rs` | `ModelMember`, `roster`, `ensure_model_member`, `set_harness_models` |
| Domain types | `crates/core/src/types.rs` | `Message`, `Channel`, `Member`, `Seq`, `BatchId` |
| Data directory (app side) | `crates/app/src/agents.rs` | `data_dir` (`CHAOS_CONTROL_DATA`, then OS data dir) |

Details: [ARCHITECTURE.md#storage-model](ARCHITECTURE.md#storage-model).

## Core: agent and harness plumbing

| Concern | File | Key types / entry points |
| --- | --- | --- |
| Core handle and actor | `crates/core/src/host.rs` | `Core`, `HostConfig`, `AgentSpec`, `Clock` |
| Delivery engine and events to the app | `crates/core/src/engine.rs` | `Engine`, `Effect`, `CoreEvent`, `AgentStatus` |
| Adapter boundary | `crates/core/src/adapter.rs` | `AgentAdapter`, `AgentSession`, `Caps`, `OpenSession`, `AgentEvent` |
| Harness presets and detection | `crates/core/src/harness.rs` | `HarnessPreset`, `detect`, `adapter_for`, `ModelInfo` |
| ACP adapter | `crates/core/src/acp.rs` | `AcpAdapter`, `AcpTransport`, `SpawnTransport` |
| ACP v2 adapter | `crates/core/src/acp_v2.rs` | ACP v2 session and approval handling |
| Native OpenCode v2 adapter | `crates/core/src/opencode_v2.rs` | `OpenCodeV2Adapter`, `start_server`, `open_session`, `SseParser` |
| Devin adapter | `crates/core/src/devin.rs` | Hub-backed adapter |
| Fake adapter for tests | `crates/core/src/fake.rs`, `crates/core/src/bin/fake-acp-agent.rs` | Fake transports/process |
| Chat MCP tools and HTTP bridge | `crates/core/src/mcp.rs` | `tools`, `call_tool`, `serve`, `run_bridge` |
| Approvals and outcomes | `crates/core/src/approval.rs` | `ApprovalRequest`, `ApprovalOutcome`, `ApprovalBroker` |
| Policy, reactions, emoji, notices, model cache | `crates/core/src/{policy,reaction,emoji,notify,model_cache}.rs` | Room policy, reactions, ping answers, cache I/O |

Details: [ARCHITECTURE.md#agents](ARCHITECTURE.md#agents),
[ARCHITECTURE.md#agent-state](ARCHITECTURE.md#agent-state),
[ARCHITECTURE.md#messages-arrive-whole](ARCHITECTURE.md#messages-arrive-whole).

## UI components and tokens

`crates/ui/src/`: `theme.rs` (`Theme`, `ActiveTheme`), `tokens.rs` (`TypeStyle`,
`Elevation`), `pane.rs` (`PaneState`, `Pane`), `text_input.rs` (`TextInput`),
`tooltip.rs`, `icon.rs` (`IconName`, `brand_icon`), `assets.rs` (`Assets`, font
loading), `editing.rs` (grapheme/word navigation), `glass.rs`, `pill.rs`,
`text.rs`, `cursor_animation.rs`. See
[ARCHITECTURE.md#design-tokens](ARCHITECTURE.md#design-tokens).

## Tests and fixtures

- Unit tests live beside code (`#[cfg(test)] mod tests`), including
  `crates/core/src/store.rs`, `crates/core/src/opencode_v2.rs`,
  `crates/app/src/store.rs` and `crates/app/src/workspace.rs`.
- End-to-end core scenarios: `crates/core/tests/*.rs` — `scenarios.rs`,
  `replay.rs`, `invariants.rs`, `v2.rs`, `opencode_v2_approval.rs`,
  `opencode_v2_mcp_live.rs`, `mcp.rs`, `mentions.rs`, `reactions.rs`,
  `nicknames.rs`, `stances.rs`, `cancel.rs`, `models.rs`,
  `approval_real_orchestrator.rs`.
- Recorded harness transcripts: `crates/core/tests/fixtures/*.ndjson`;
  shared test setup: `crates/core/tests/common/mod.rs`.
- Sample data for the app: `crates/model/src/sample.rs` (see
  `CHAOS_DEMO_MESSAGES` / `CHAOS_DEMO_CHANNELS` in `README.md`).
- CI: `.github/workflows/ci.yml`; local hooks: `.githooks/`.

## Keeping this map true

No CI check. Any PR that adds, moves or renames a module updates this file in
the same PR. See [`docs/agents.md`](docs/agents.md).
