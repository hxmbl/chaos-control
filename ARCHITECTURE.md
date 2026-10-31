# Architecture

Chaos Control is a group chat for a project's agents and people. It has to stay
fast with very long channel histories and many agents posting at once, so the
UI is native [GPUI](https://www.gpui.rs) (GPU-rendered, retained entities,
immediate-mode elements) instead of a webview.

## Crates

- `model`: plain Rust types (`Workspace`, `Channel`, `Agent`, `Message`,
  `ChannelLog`) and sample data. No GPUI, so it compiles fast and is easy to
  unit-test. Strings are `Arc<str>`, which turn into GPUI `SharedString`s
  without copying.
- `ui`: the theme (light and dark, following the OS), the design tokens,
  bundled assets and the few components the shell needs (pill, icon, tooltip,
  text input, the pane and its glass). Hand-built to match the design rather than pulling in a
  component kit.
- `app`: the binary. One view per region: `Titlebar`, `Sidebar`,
  `RightSidebar`, `Conversation`, `Composer`, laid out by the
  root `Workspace` view, plus `PaneHost` (what the pane shows), the
  `AppSettings` global and `project` (which project the launch folder is).
- `core` (`chaos-core`): the agent runtime. SQLite store, delivery engine,
  room policy, the ACP adapter and the chat MCP server. No GPUI: the app is one
  client of it. See [docs/agents.md](docs/agents.md).

## Agent state

Each agent has a `model::AgentState`, drawn as a soft glow under its tile in
Agent status. The theme maps it to a colour (`Theme::agent_state`, the
`status_*` roles), so no view hardcodes one. Status colours are the only
colour in the sidebars: agent marks are monochrome, and the accent (purple)
means "you / unread / on", never agent state:

| State | Glow | Meaning |
| --- | --- | --- |
| `Active` | green | Running and ready to read and reply. |
| `Sleeping` | blue | Idle; needs a ping before it picks anything up. |
| `Unresponsive` | red | Missing or not answering. |

Beside the state, `Agent::presence` holds one line from the runtime ("working:
running tests", "needs sign-in: ...", "request pending: X (#3)").
`Agent::status_line()` joins them for the Agent Status tooltip. Core reports
both through `CoreEvent::Agent`; presence is never part of the chat log.

## Agents

When an `agents.json` exists **or** any known harness is installed
(auto-detection, `chaos_core::harness`), `main.rs` starts `chaos-core`
in-process on its own two-thread tokio runtime (`agents::start`), works out
the launch folder's project (see Storage model below) and hands the link to
`ChatStore::attach_core`. No hand-written `agents.json` is needed. From then on the sidebar shows that project's
channels from core and #main is the project's own `main` channel:

- composer posts in #main go to `Core::post` and appear when core reports them
  back (`CoreEvent::Message`), so the log only ever shows what core stored;
- `ChatStore::apply_core_event` maps core messages onto `model::Message`
  (agents to `Author::Agent`, runtime notices to `Author::System`, replies to
  `ReplyRef`) and glow/presence onto the agent's pill;
- history is loaded once at attach and deduplicated against the event stream
  by core message id;
- members are models running through a harness (Codex, fx, Mistral Vibe,
  Claude Code, ...): the roster is `agents.json`'s `members`, seeded once
  from the detected harnesses and then edited in Agent Status (remove on
  hover, "Add a model" opens the model picker in the pane layer,
  `model_picker.rs`), which writes it back (`agents_file.rs`). Members whose
  harness isn't installed stay out of the room. Model lists come from an
  on-disk cache probed in the background on core's tokio runtime, never
  from the UI thread. Presets, detection, model discovery and the picker
  are in [docs/agent-adapters.md](docs/agent-adapters.md);
- the same binary is the stdio MCP bridge for agents that can't take HTTP:
  `chaos-control mcp-bridge` (URL and token in its environment).

Only #main is wired to post through core so far; other channels arrive with
channel management. The delivery, batching, obligation and permission rules are
in [docs/agents.md](docs/agents.md).

## Storage model

The folder Chaos Control is launched from is the project. A project contains
channels; each channel is a persistent group conversation.

**Identity travels with the folder; state stays in the app.**

| What | Where | Keyed by |
| --- | --- | --- |
| Project id (UUID) and display name | `<folder>/.chaos/project.toml` | n/a |
| Projects, channels, messages, project and channel membership, cursors, delivery state, sessions | `<data>/core.sqlite` | project UUID, channel id |
| Model members (harness + model) and each project's roster, harness model lists | `<data>/core.sqlite` (migration 7) | member id, project UUID |
| Optional overrides: harness commands, an explicit roster, original-shape agents | `<data>/agents.json` | harness id / handle (global) |

`<data>` is the app data directory (see Agents above). The file holds nothing
else, so no chat state ever lands in a repository, and agents working in the
folder can't edit membership or history by editing files.

```toml
# Chaos Control project identity. ...
version = 1
id = "0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59"
name = "starpoint"
```

**Ownership.** Agents (`members`) are global; a model member is one per
`(harness, model)`. Membership is per project
(`project_members`) and per channel (`memberships`). Channels belong to one
project: names are unique within it (two projects can each have a #main), and
each channel has a per-project ordinal, its `::N`. Cursors, batches, triggers,
obligations and sessions hang off channel ids as before.

**Schema (core migration 2).** `projects (id TEXT UUID, name, root_path,
created_at)`, where `root_path` is the folder it was last opened from and one
folder maps to at most one project. `channels` gains `project_id` and
`ordinal`, with `UNIQUE(project_id, name)` and `UNIQUE(project_id, ordinal)`;
moving uniqueness off `name` is a SQLite table rebuild that keeps every
channel id, so messages, cursors, obligations and sessions are untouched.
`project_members (project_id, member_id, joined_at)`. A v1 database migrates
into one project named "Imported" (fresh UUID, no folder) holding every
existing channel, with each channel's members as project members. Migrations
run with foreign keys off around them (SQLite can't drop a referenced table
otherwise) and each checks `pragma_foreign_key_check` before it commits.
Migration 3 adds `projects.unclaimed_import`, set only on that "Imported"
project, so it can be adopted by one folder, once (see Relinking imported
history below).

**`::N`.** A channel's ordinal is assigned when it is created: the project's
highest so far plus one, starting at 1 (so a new project's #main is `::1`).
It is never reused and renaming doesn't change it. Imported channels are
numbered in their original id order. The header (top of the chat column)
shows it under the channel name:

```text
# main
~/Projects/everything ::1
```

**Renaming** changes the display name only, for projects (edit `name` in
`project.toml`, or `rename_project`) and channels (`rename_channel`): ids,
ordinals and history stay.

**Trust rules.** `project.toml` is a hint, never a key (`project::decide`,
unit-tested as a pure function):

| The folder's file | What core knows | Result |
| --- | --- | --- |
| missing (or unreadable) | this folder | relink: open that project, rewrite the file |
| missing (or unreadable) | nothing | new project, fresh UUID, named after the folder |
| missing | nothing (or an empty project), with the "Imported" project unclaimed | fresh project open; ask: Use my earlier history here / Start fresh |
| UUID X | this folder as X | open X; the file's name is the display name |
| UUID X | this folder as Y | keep Y open and ask in the pane: Keep current / Use the new one / Treat as a new project |
| UUID X | nothing about X | new local project under X (a cloned repo that shared its file) |
| UUID X | X, from a folder that is gone | it moved: open X here |
| UUID X | X, from a folder that still claims it | a copy: open a separate project and ask (Keep it separate / Use X here too) |

**Relinking imported history.** A folder with no file and no project of its
own (or only an empty one from an earlier unanswered ask), while the
"Imported" project is still unclaimed and has no folder, opens a fresh
project and asks in the pane: *Use my earlier history here* adopts the
import (`Core::adopt_import`: it takes this folder as its root and the
folder's name, the file is written with its UUID, the flag clears, and the
empty fresh project is deleted) or *Start fresh* writes the file for the
fresh project, so this folder never asks again. Only an unrooted, unclaimed
import is ever offered or adoptable; a project that just lost its folder to
another one is not. "Use X here too" on the copy question likewise deletes
the separate project it opened if nothing was said there
(`Core::forget_empty_project`).

A file the app can't read (bad TOML, a future `version`, an id that isn't a
UUID) is never overwritten. Nothing switches before the user answers; "Keep
current" rewrites the file to name what is open, so it stops asking.

Permissions are enforced in core, not by where files sit:

- joining a channel needs membership of its project, checked by a trigger on
  `memberships`, so no code path can skip it (`not_a_project_member`);
- posting never grants it: a human posting joins a channel only inside a
  project they already belong to;
- an agent's chat tools resolve channel names, searches and message ids only
  inside its session's project; anything else answers `not_found`;
- granting is explicit and only the host does it (`Core::human`,
  `Core::add_agent`, `Core::join_project`); the app grants the person at the
  keyboard and the launch project's roster members (plus any original-shape
  `agents.json` agents) the launch project.

**Git.** `.chaos/` is appended once to the repository's `.git/info/exclude`
(found by walking up; a worktree's `.git` file is followed to the common git
dir), never to `.gitignore`. A shared UUID is shared identity, not a shared
chat: each install keeps its own database until a sync layer exists.

Not built yet: the "Share project identity" action (committing
`project.toml` on purpose), sync, and a permission editor.

## State

- One `Entity<ChatStore>` owns the chat state: the workspace, the selected
  channel and an append-only `ChannelLog` per channel. Views hold a handle to it
  and never keep their own copies of the data.
- The store emits precise events (`Selected`, `Appended { channel, range }`).
  Views subscribe and do only the work the event needs: the conversation splices
  new rows onto its list, the composer swaps its placeholder.
- Rendering reads straight from the store; no cloning of message bodies.

## Messages arrive whole

Agents post a complete message when their response is done; nothing is streamed
token by token. Incoming messages (one or a batch) go through
`ChatStore::append`, which pushes onto that channel's `ChannelLog` and emits
`Appended`. If that channel is on screen, `Conversation` calls
`ListState::splice(len..len, n)`, so only the new rows are measured; history
already laid out is left alone. Core's events are received on a GPUI task and
handed to `append` on the main thread (see Agents above).

## Search

The titlebar field (`cmd/ctrl-K`) searches channel names and message text
across every channel. The UI only knows the `model::SearchProvider` trait: each
keystroke takes a `SearchCorpus` snapshot from the store (it clones `Arc`s, not
messages; `ChannelLog` is copy-on-write) and runs the provider on
`cx.background_spawn`, so a 100k-message history never blocks typing. A newer
query sets the previous one's cancel flag and drops its task, so stale results
never land.

At rest the field is the pill in the titlebar. Focusing it (a click or
`cmd/ctrl-K`) grows that pill into the search panel over 140ms: the same spot
and centre, widening to half the window (560 to 760px), with one field on top
and the results under it. There is never a second input row. Under the field:
the highlighted result as a card, its actions as rows (as in the mock: "New
chat", "Ask an agent to the side", "Link this in main"), a divider, the other
results as rows, and a "…" row when more are hidden (channels first, then
messages newest first, capped at 20). Up/down move, Enter or a click jumps to
the channel and scrolls to the message. Escape clears the query, and on an
empty field closes the panel; a click outside closes it too. Closing hands the
keyboard back to the composer and turns the panel back into the pill.
Opening is explicit (a click on the pill, `cmd/ctrl-K`, or typing into the
field); focus moving elsewhere in the window also closes it, but switching to
another app doesn't. Only one of pill or panel draws the `TextInput` at a time.
`cmd/ctrl-K` with the panel already open selects the query.

Each `SearchResult` can carry `SearchAction`s for the coming command palette;
the panel already draws them and emits `SearchEvent::Action`, but nothing
produces them yet. Today's provider, `SubstringSearch`, is a plain
case-insensitive scan; ranking, semantic search or commands can replace it
without touching the UI.

## The pane

A fixed, translucent glass overlay that shows what you need or what you
clicked on without taking you away from the chat behind it: long messages
today, settings, and later attachments, reviews and long messages to send. It
never moves and there is never more than one.

- **Content is model data.** `model::PaneContent` (GPUI-free) says what is
  shown: `LongMessage`, `Attachment`, `Settings`, `Review`, `Compose`,
  `Search`. Each knows its title and its quick actions (`PaneAction` /
  `PaneActionKind`), so the agent runtime can open a pane by building one and
  calling `PaneHost::open`. The UI only decides how each body looks.
- **Element and state.** `ui::Pane` is the element: a red macOS-style close dot
  at the top left (12px, 12px from the corner; an × shows on hover) with a
  header row beside it like a macOS title bar: the title (17px semibold,
  8px past the dot, ellipsis when narrow) or a custom header such as search's
  field, centred on the dot. Then a scrollable body that takes any element, and full-width rounded quick-action
  rows under a 1px hairline divider. `ui::PaneState` is the bookkeeping, unit-tested
  without a window: it remembers what had focus when the pane opened and hands
  it back on close; opening over an open pane swaps the content in place (no
  second animation, focus still returns to where it was before the first).
- **Closing.** The close dot, Escape (`Pane` key context) and a click
  outside. Focus goes back to whatever had it, unless the click that closed
  the pane focused something itself (the search pill, the composer), which
  wins; with nothing to go back to, the composer gets it.
- **Where it sits.** `pane_frame` (in `pane_host.rs`): between the docked
  sidebars, from under the titlebar to just above the composer, centred on the
  window like the chat column when that clears both sidebars, as wide as the
  column + 160px (at most 1120px), and simply the space left on a narrow
  window. It reflows as sidebars dock, collapse or slide; a peeking sidebar
  slides in over it. Tall content scrolls inside the body; short content
  makes a short pane.
- **Motion.** 140ms fade and scale-in (ease-out cubic), the same timing as the
  search grow and the sidebar peek. Closing is instant.
- **The glass, faked.** gpui 0.2.2 has no backdrop blur, so `ui::Glass`
  (`glass.rs`, `Theme::glass`) builds it from a flat translucent fill (about
  92% opaque light, 91% dark), a
  scrim over the area behind the pane that knocks the chat back the way a blur
  would (light: the canvas colour at 42%; dark: black at 38%), a hairline
  border, a 1px bright highlight along the top inner edge, and a soft, wide
  two-layer shadow. The scrim covers only the pane's box, never the sidebars
  or the composer. All of it lives in `Glass`: when GPUI gets a real backdrop
  blur, add it there, lower the fill's alpha and drop the scrim.

### Long messages

A message over 600 characters or 8 lines (`model::pane::is_long`) shows
collapsed in the chat: its first 4 lines or 320 characters, cut between words
and ending in "…" (`model::pane::preview`), then an "Open" button and a word
count. "Open" shows it in full in the pane, with the author and time, and two
quick actions: **Reply** closes the pane and puts a reply reference ("Replying
to Claude Code: …", cancellable) above the composer field; the sent message
carries a `ReplyRef` snapshot (author + one quoted line), drawn as a quote line
above its body, so drawing a reply never searches the log. **Copy** copies the
text and reads "Copied" for a moment.

### Settings

The left sidebar's Settings opens the settings pane: appearance (follows the
OS), the search-in-pane switch, the long-message rule, shortcuts, and
placeholders for agents and notifications. `model::Settings` lives in the
`AppSettings` global, in memory only until there is a storage layer.

### Search in the pane (experiment)

Off by default; turn it on in Settings or start with `CHAOS_SEARCH_IN_PANE=1`.
Search then opens in the pane instead of the dropdown, anchored to the top of the pane's area so it
grows downwards as results arrive: the field takes the
title's place next to the close dot, results fill the body (the highlighted one
as a card), and the highlighted result's actions are the quick actions, led by
"Go to message" / "Go to channel". It is the same `Search` entity, provider,
cancellation, keys and jump as the dropdown; only the drawing changes. The
titlebar keeps a quiet stand-in pill showing the query while the field is in
the pane. Escape still clears the query first, then closes. cmd/ctrl-K closes
any other open pane first, so there is still only one.

## Design tokens

Every size the shell draws with comes from `ui::tokens`; colours come from
`ui::Theme` by role. Views don't write one-off `px(..)` values.

| Token | Values |
| --- | --- |
| `space` | 2, 4, 6, 8, 12, 16, 24, 32 |
| `text` | `CAPTION` 12/16, `SMALL` 13/18, `BODY` 15/22, `TITLE` 17/24 semibold; weights regular, medium, semibold only |
| `radius` | `SM` 6 (tooltips), `MD` 8 (buttons, rows, agent tiles, send), `FIELD` 10 (search field, composer), `LG` 14 (sections, search panel), `XL` 20 (sidebars, pane); concentric (XL → LG → MD each step the 6px inset; LG wraps FIELD with the search panel's 4px inset). `rounded_full()` only for dots, avatars, status indicators and switches |
| `size` | `CHIP` 22, `BUTTON_SM` 28, `BUTTON` 32, `ROW` 36, `ROW_LG` 44, `HAIRLINE` 1 |
| `icon` | `SM` 12, `MD` 16, `LG` 20; Phosphor regular weight only |
| `Elevation` | `None`, `Float` (docked sidebars, composer, search), `Overlay` (a peeking sidebar); `Theme::elevation` |

Apply a type step with `.type_style(text::BODY)` (`ui::TypeStyleExt`).
Colour roles worth knowing: `accent` and `on_accent` for "you / unread / on",
`status_active` / `status_sleeping` / `status_unresponsive` for agent state.

Surface rules:

- Separation comes from one signal, never two. Panels have a hairline and a
  shadow; sections inside them are a background step (`sunken`) with no
  outline; rows inside sections are borderless rounded rows that show `hover`
  under the pointer and `selected` when chosen.
- The left sidebar's channel section hugs its rows (channels, "+", Archived)
  and grows with each new channel; once it reaches the quick links it stops
  and the list scrolls inside it. The quick links sit straight on the panel
  surface with hover feedback only.
- Text fields (the search field, the composer) keep a hairline border, because
  it says "you can type here". Nothing else is outlined.
- Dividers are 1px hairlines.
- Optical exceptions (the pane's close-dot centring, the switch knob) stay as
  named constants beside their component, with a comment.

## Rendering

- **Virtualized lists.** The message history uses GPUI `list` with a
  bottom-aligned `ListState`: only rows in view (plus 600px of overdraw) are
  laid out and painted, and row heights are cached, so 100k messages cost about
  the same per frame as 20. Try it with `CHAOS_DEMO_MESSAGES=100000`. The
  channel list uses `uniform_list` (fixed row height, so no measuring at all).
- **Cached views.** The root wraps each region in `AnyView::cached`, so a view
  re-renders only when it is notified itself. Nothing animates at rest: the
  only animations are the sidebar slide, the search grow and the pane fade,
  each of which stops when it finishes. (The wireframe's catch-up card, with
  its endless shimmer, is not drawn until summaries exist.)
- **No work in hot paths.** Theme colours are `Copy`; icons are embedded SVGs
  that GPUI rasterises once per size and caches; fonts are compiled in. Row
  rendering does no I/O and no string formatting beyond the timestamp.
- **Off the main thread.** Anything slow (network, disk, search indexing,
  summarising for catch-up) belongs on `cx.background_spawn`, handing results
  back to the store. The main thread only applies results and renders.

## Platform chrome

- macOS: transparent titlebar with the native traffic lights placed on the 52px
  bar; the titlebar drags the window and double-click follows the system setting.
- Linux: the window asks for client-side decorations. When the compositor
  agrees, the app draws minimise, maximise and close, a 1px border and resize
  handles on each edge; when it insists on server decorations, those are skipped.
- Window: 1440×900 by default, 760×520 minimum.
- Sidebar toggles: titlebar buttons at either end (the left one just past the
  macOS traffic lights) and `cmd/ctrl-alt-[` / `cmd/ctrl-alt-]`. See Sidebars
  below for what they do at each width.
- Layout: both sidebars float 12px from the window edges and run from just
  under the titlebar to 12px above the bottom, so they grow with the window
  (the channel list and mentions take the extra height). The
  conversation and composer share one centred column that widens with the
  window, from 720px up to 960px.

## Sidebars

Both sidebars collapse on their own when the window gets too narrow, Notion
style, and a collapsed one is never gone: hovering the window edge slides it
back in over the content. The rules live in `panels.rs` as plain state
(`plan`, `Peek`, `Panels`) with unit tests; `Workspace` feeds it the window
size, pointer moves and focus, and draws what `Panels::frame` returns.

Each sidebar has a mode, `Auto` by default:

| Mode | Docked when | Set by |
| --- | --- | --- |
| `Auto` | the chat column keeps at least 560px beside it | default; showing a sidebar where it fits |
| `Pinned` | the column keeps at least 360px | showing a sidebar where it wouldn't fit on its own |
| `Collapsed` | never (edge peek only) | hiding a docked sidebar |

- Breakpoints (`Auto`, 560px column): both docked from 1202px wide (the old
  1200px cut-off); below that the right one collapses first; the left one
  alone needs 870px. With the left one hidden by hand, the right one alone
  needs 932px. An `Auto` sidebar that collapsed comes back only 24px past its
  breakpoint, so dragging a window edge across the line doesn't make it flap.
- Toggle on a docked sidebar: it collapses and stays collapsed at every width
  (`Collapsed`) until toggled again. Resizing never changes a mode.
- Toggle on a collapsed or peeking sidebar: it docks now. If it fits it
  becomes `Auto` (so later narrowing collapses it again); if it doesn't, the
  user asked for it anyway, so it becomes `Pinned` and stays docked through
  resizes until hidden. Toggling a peek docks it in place: the panel stays
  put and only the column slides over.
- Space: pinned sidebars are placed first, newest pin first, then `Auto`
  ones, left before right. So pinning the right sidebar on a narrow window
  tucks the left one away (it comes back when there's room), and when both
  are pinned but only one fits, the one pinned last wins. The window minimum
  (760px) always fits one pinned sidebar.
- Edge peek: a 10px strip at each window edge, starting under the 52px
  titlebar, so on macOS it never overlaps the traffic lights or the titlebar
  toggle. Entering it (with no mouse button held, so a text drag off the edge
  doesn't trigger it) slides the collapsed sidebar in over 140ms as a floating
  overlay: deeper shadow, no reflow, fully interactive, clicks don't fall
  through. While open, the panel plus the strip between it and the edge plus
  12px past its outer edge count as inside, so there is no flicker between
  zone and panel. Leaving (or the pointer leaving the window) starts a 350ms
  grace; coming back cancels it. It stays open while keyboard focus is inside
  it or while something inside holds it (`Panels::set_held`, for menus and
  popovers); the grace starts when that ends.
- Motion: docking and collapsing slide the panel off its edge and reflow the
  column over 180ms (ease-out cubic), reversible mid-slide.

## Shortcuts

Rule (pre-release, standing): stay conventional. If a combo already means
something to most people, it does that here, even if one of our features has to
move. Chat apps (Slack, Discord) and desktop text editing win over IDE habits
where they clash, and text fields never lose the standard editing keys (copy,
paste, cut, undo, select all, word and line navigation). `secondary` is cmd on
macOS and ctrl elsewhere.

| Keys | Does | Where |
| --- | --- | --- |
| `cmd/ctrl-K` | Open search (again: select the query) | Anywhere |
| `cmd/ctrl-alt-[` | Show/hide the left sidebar | Anywhere |
| `cmd/ctrl-alt-]` | Show/hide the right sidebar | Anywhere |
| `up` / `down` | Move through results | Search |
| `enter` | Jump to the highlighted result | Search |
| `escape` | Clear the query; on an empty field, close | Search |
| `escape` | Close the pane | Pane |
| `enter` | Send | Composer |
| `cmd/ctrl-Q` | Quit | Anywhere |

Text editing in the composer and search field (`ui::TextInput`; GPUI 0.2.2
ships no text field, only an example, so these are ours). Single-line fields,
so "line" means the whole text.

| Action | macOS | Linux / Windows |
| --- | --- | --- |
| Move by character / select | `left` `right`, `shift-` | same |
| Move by word | `alt-left` / `alt-right` | `ctrl-left` / `ctrl-right` |
| Select by word | `alt-shift-left` / `right` | `ctrl-shift-left` / `right` |
| Start / end | `cmd-left` / `cmd-right`, `cmd-up` / `cmd-down`, `home` / `end`, `ctrl-a` / `ctrl-e` | `home` / `end`, `ctrl-home` / `ctrl-end` |
| Select to start / end | `cmd-shift-left` / `right` (and `up` / `down`), `shift-home` / `end` | `shift-home` / `end`, `ctrl-shift-home` / `end` |
| Delete character | `backspace`, `delete`, `ctrl-h`, `ctrl-d` | `backspace`, `delete` |
| Delete word | `alt-backspace` / `alt-delete` | `ctrl-backspace` / `ctrl-delete` |
| Delete to start / end | `cmd-backspace` / `cmd-delete`, `ctrl-k` (to end) | (no common binding) |
| Select all | `cmd-A` | `ctrl-A` |
| Copy / cut / paste | `cmd-C` / `X` / `V` | `ctrl-C` / `X` / `V`, `ctrl-insert` / `shift-delete` / `shift-insert` |
| Undo / redo | `cmd-Z` / `cmd-shift-Z` | `ctrl-Z` / `ctrl-shift-Z`, `ctrl-Y` |
| Select word / all | double / triple click | same |
| Emoji & symbols | `ctrl-cmd-space` | (the system's own) |

Words: a run of letters, digits and `_` (accents and emoji join their word),
or a run of punctuation; spaces are skipped first. Word-right stops at the end
of the word, as on macOS and GTK (Windows stops at the next word's start).
Undo groups a burst of typing (or of single deletes) into one step, split by a
pause over a second, a cursor move or a switch between typing and deleting;
paste, cut and word/line deletes are always their own step. Sending a message
or clearing the search empties the history. `ctrl-K` stays search on Linux and
Windows; on macOS it's the Cocoa kill-to-end key in a text field, since search
is `cmd-K` there.

Reserved, not bound yet: `cmd/ctrl-F` and `ctrl-shift-P` (also focus the bar
once it doubles as the command palette), `cmd/ctrl-[` / `cmd/ctrl-]` (back and
forward through channel history), `cmd/ctrl-B`, `I`, `U` (bold, italic,
underline in the composer).
