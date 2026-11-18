# Agent adapters: harnesses, detection and models

How Chaos Control finds the coding agents installed on a machine, starts
them, and picks the model each one runs. The room rules (wakes, delivery,
obligations) are in [`agents.md`](agents.md); this file covers what sits
under them.

> **Not live-verified.** Everything below is built and covered by tests
> against a scripted fake ACP agent (`crates/core/tests/models.rs`), but
> none of it has been run against a real harness yet. Treat launch commands
> and model details as theoretical until the manual checks at the end pass
> on a real machine.

## Members are models

A chat member is a **model** ("Claude Opus 5.5", "Devstral") running through
a **harness** (Codex, fx, Mistral Vibe, ...). The harness only decides how
the process starts and which protocol it speaks; the room treats every
member the same. One harness can host several members, each with its own
adapter, processes and sessions; `(harness, model)` is unique, so the same
model through the same harness is one member everywhere. A member whose
model is empty runs the harness default. The roster, `agents.json` and the
write-back rules are in [`agents.md`](agents.md#running-it).

Code: `crates/core/src/harness.rs` (presets, detection, catalog, ranking),
`crates/core/src/roster.rs` (model members and rosters),
`crates/core/src/acp.rs` (ACP adapter, model option), `crates/core/src/
acp_v2.rs` (v2 draft), `crates/core/src/model_cache.rs` (on-disk lists),
`crates/core/src/devin.rs` (workspace trust), `crates/app/src/agents.rs`
(startup) and `crates/app/src/model_picker.rs` (the picker).

## Harnesses

`harness::PRESETS`, in Hxmbl's priority order (2026-10-30). **ACP only:**
the native Codex app-server adapter was removed in STRM-542, so every
harness — Codex included — is driven by the generic ACP adapter (v2 draft
offered first, v1 fallback; negotiation in
[`agents.md`](agents.md#acp-version-negotiation)). The room and delivery never
branch on the brand; the one brand-specific step is Devin's workspace-trust
setup (see below). Launch commands come from the official ACP agent registry.

| # | Id | Harness | Detected by | Launch | Model selection | Install |
|---|---|---|---|---|---|---|
| 1 | `codex` | Codex | `codex-acp` | `codex-acp` | ACP model config option | `npm i -g @zed-industries/codex-acp` |
| 2 | `fx` | fx | `fx` | `<bin> acp` | ACP model config option | fx setup script |
| 3 | `vibe` | Mistral Vibe | `vibe-acp`, `vibe` | `vibe-acp` (sibling of `vibe`) | ACP model config option | Vibe install script |
| 4 | `claude-code` | Claude Code | `claude`, `claude-agent-acp` | `claude-agent-acp`, else `npx -y @agentclientprotocol/claude-agent-acp@0.99.1` | ACP model config option | `npm i -g @anthropic-ai/claude-code` |
| 5 | `copilot` | Copilot CLI | `copilot` | `<bin> --acp` | ACP model config option | `npm i -g @github/copilot` |
| 6 | `opencode` | opencode | `opencode` | `<bin> acp` | ACP model config option | opencode install script |
| 7 | `devin` | Devin | `devin` | `<bin> acp` | ACP model config option | Devin for Terminal |
| 8 | `cursor` | Cursor Agent | `cursor-agent` | `<bin> acp` | ACP model config option | Cursor install script |
| 9 | `gemini` | Gemini CLI | `gemini` | `<bin> --acp` | ACP model config option | `npm i -g @google/gemini-cli` |

`<bin>` is the detected binary itself. Claude Code doesn't speak ACP itself;
the `claude-agent-acp` adapter binary is used when installed, else it runs
through `npx` (pinned to 0.99.1), which needs Node. With neither, Claude
Code counts as not installed. A harness given an explicit `command` in
`agents.json` counts as installed even when detection didn't find it.

Chaos Control adds its chat MCP server when a session can take it (HTTP when
the agent advertises `mcpCapabilities.http`, else the stdio bridge); a harness
that can take neither runs without chat tools. Otherwise it leaves a harness's
own settings alone.

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
the detected binary itself, a sibling binary (`vibe-acp` next to `vibe`), or
the adapter / `npx` path for Claude Code. Detection only checks files; it
never runs a harness. Turn it off with `"detect": false` in `agents.json`
or `CHAOS_CONTROL_DETECT=0`. The app starts core when an `agents.json`
exists **or** at least one harness is detected (see "Running it" in
[`agents.md`](agents.md#running-it)).

## Model discovery

Each preset carries a fallback model list, shown until the harness reports
its own: Claude Code names from T3 Code's model manifest of 2026-10-28
(plus "Default model"); everyone else only "Default model" (Vibe's preset
names "Devstral (Vibe default)"). Codex had T3 names too until STRM-542
moved it to `codex-acp`; its preset now has only "Default model".

Each harness's own list is cached on disk, per harness and version
(`chaos_core::model_cache`, `<data>/models/<harness>.json`):

- **When core starts**, every installed harness whose cached list is
  missing, stale (older than a day) or from another version is probed once,
  in the background (`ChatStore::refresh_models` → `Core::probe_models` →
  `AgentAdapter::list_models`). The answer is written to the cache and
  lands in the picker. A live session that reports its harness's models
  also updates the cache.
- **The picker reads the cache.** Opening it shows the lists the app
  already has, at once; it never starts a harness. It only asks for a
  background refresh of lists that are still stale; a harness whose probe
  failed is left alone for five minutes.
- **Version** is read from the launch command without running it: the
  resolved binary path, its size and modification time, and the launch
  arguments (Claude Code's `npx` package is pinned there). Upgrading a
  harness replaces its binary, so its list is probed again. A stale list
  is still shown until the new one arrives.

A probe spawns the harness once, always on core's own tokio runtime:
`initialize`, then a throwaway `session/new` with no chat tools, reading
the session's model selector from `configOptions` (the `select` option
whose category is `model`, else the one with id `model`), then closing the
session straight away. Times out after 30 s. A failure (signed out,
timeout) caches nothing. Discovery runs on core's runtime — not the app's
GPUI executor — because the adapters do tokio I/O, which panics without a
reactor (`discovery_can_be_awaited_from_outside_the_tokio_runtime` in
`tests/models.rs`).

### Selecting the model for a session

After `session/new`, if the member has a model and the harness offers it in
the model option, `session/set_config_option` sets it. If the harness
doesn't offer it, the session runs the harness default and the member's
presence says so ("running its default"). A member with no model sets
nothing.

### Devin: workspace trust

`devin acp` refuses to work in a folder the member hasn't trusted (Devin CLI
3000.11.234 hung 100 s in one). It has no per-folder flag, so before every
Devin session start and model lookup the ACP adapter adds the launch folder
to Devin's own trust file (`crate::devin::ensure_trusted`):
`$XDG_DATA_HOME/devin/cli/trusted_workspaces.json` (else
`~/.local/share/devin/cli/trusted_workspaces.json`) as
`{"trusted_paths": [...]}`, canonicalised, with a subfolder of a trusted
path counting as trusted. Existing entries and other keys are kept; a
malformed file is left untouched (warning on stderr, session starts
anyway). Devin is recognised by the agent binary's name; other harnesses
are untouched.

## The model picker

Agent Status lists the project's roster members: model name, harness mark,
status dot, a remove button on hover, and an "Add a model" row that opens
the picker in the pane layer.

- The picker lists **models**, not harnesses. `harness::Catalog` groups the
  same model offered by several harnesses into one row. Lists come from the
  cache; opening the picker never starts a harness.
- A search field on top, a harness filter row ("All" plus one chip per
  installed harness), then the ranked list (`harness::rank`, ported from
  T3 Code). Up/Down move the highlight, Enter adds, Escape backs out of a
  step, then closes.
- A row one installed harness offers is added straight away. A row several
  installed harnesses can run opens a required second step listing them
  (Hxmbl, 2026-10-31: no auto-pick, no silent default); nothing is added
  until a harness is picked. With a harness filter selected, that harness
  is the choice. A harness that already runs that model as a member is left
  out of the choice.
- Roster members whose harness isn't installed are listed at the bottom,
  greyed, "not installed · <harness>", and can't be added. They leave the
  room entirely (see "Harness not installed" in [`agents.md`](agents.md)).

### Credit: T3 Code (MIT)

[T3 Code](https://github.com/pingdotgg/t3code) is MIT licensed. Borrowed:
the picker's structure (search on top, harness filter, ranked list — the
side rail becomes a row in our narrower pane), the search ranking
(`harness::rank`, ported from `modelPickerSearch.ts`), and the fallback
model names for Claude Code (T3's manifest of 2026-10-28).

## Manual checks (per harness)

None of these has been run yet. Each needs the harness installed and signed
in. Protocol smoke test against the raw harness (swap `fx acp` for each
launch command; ACP harnesses answer `initialize` on stdin):

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}' | fx acp
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
   green after `@handle hello`; it answers in the room through `chat_send`.
2. "Add a model" opens at once (no harness starts); its filter chip stops
   saying "usual models" once the background probe has listed its own and
   `<data>/models/<harness>.json` exists. Picking a model adds a member
   that actually runs that model, and `agents.json` gains the entry.
3. A model two harnesses offer asks which harness (keyboard and click) and
   adds nothing until one is picked.
4. Removing the member takes it out of Agent Status and `agents.json`; its
   old messages stay, signed "Previous agent".
5. Uninstall a harness and restart: its members are gone from Agent Status
   and greyed in the picker; reinstall and restart: they're back.
6. Signed out: the member goes red with one note and no retry loop.

## Tests

`cargo test -p chaos-core --test models` covers, against the fakes: an ACP
member setting its model through the session model option; a model the
harness doesn't offer running the default and saying so; a default member
setting nothing; discovery opening and closing a throwaway session without
chat tools; a signed-out harness's discovery caching nothing; one harness
hosting two members; discovery awaited off the tokio runtime; and the
probe's on-disk cache. Detection, catalog grouping and ranking have unit
tests in `harness.rs`; in the app, `agents_file.rs` covers the `agents.json`
write-back and `agents.rs` covers not-installed members.
