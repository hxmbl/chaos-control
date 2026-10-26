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
- `ui`: the theme (light and dark, following the OS), bundled assets and the
  few components the shell needs (pill, icon, tooltip, text input, the pane
  and its glass). Hand-built to match the design rather than pulling in a
  component kit.
- `app`: the binary. One view per region: `Titlebar`, `Sidebar`,
  `RightSidebar` (+ `CatchUp`), `Conversation`, `Composer`, laid out by the
  root `Workspace` view, plus `PaneHost` (what the pane shows) and the
  `AppSettings` global.
- `core` (`chaos-core`): the agent runtime. SQLite store, delivery engine,
  room policy, the ACP adapter and the chat MCP server. No GPUI: the app is one
  client of it. See [docs/agents.md](docs/agents.md).

## Agent state

Each agent has a `model::AgentState`, drawn as a soft glow under its pill in
Agent Status. The theme maps it to a colour (`Theme::agent_state`), so no view
hardcodes one:

| State | Glow | Meaning |
| --- | --- | --- |
| `Active` | green | Running and ready to read and reply. |
| `Sleeping` | purple | Idle; needs a ping before it picks anything up. |
| `Unresponsive` | red | Missing or not answering. |

Beside the state, `Agent::presence` holds one line from the runtime ("working:
running tests", "needs sign-in: ...", "wanted to run X, declined").
`Agent::status_line()` joins them for the Agent Status tooltip. Core reports
both through `CoreEvent::Agent`; presence is never part of the chat log.

## Agents

When an `agents.json` exists, `main.rs` starts `chaos-core` in-process on its
own two-thread tokio runtime (`agents::start`) and hands the link to
`ChatStore::attach_core`. From then on #main is core's `main` channel:

- composer posts in #main go to `Core::post` and appear when core reports them
  back (`CoreEvent::Message`), so the log only ever shows what core stored;
- `ChatStore::apply_core_event` maps core messages onto `model::Message`
  (agents to `Author::Agent`, runtime notices to `Author::System`, replies to
  `ReplyRef`) and glow/presence onto the agent's pill;
- history is loaded once at attach and deduplicated against the event stream
  by core message id;
- the same binary is the stdio MCP bridge for agents that can't take HTTP:
  `chaos-control mcp-bridge` (URL and token in its environment).

Other channels stay local until core grows channel management. The delivery,
batching, obligation and permission rules are in
[docs/agents.md](docs/agents.md).

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
  at the top left (an × shows on hover), a large title or a custom header, a
  scrollable body that takes any element, and full-width rounded quick-action
  rows under a thin divider. `ui::PaneState` is the bookkeeping, unit-tested
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
  (`glass.rs`, `Theme::glass`) builds it from a translucent fill that is a
  touch lighter at the top (a linear gradient, about 94% to 90% opaque), a
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

## Rendering

- **Virtualized lists.** The message history uses GPUI `list` with a
  bottom-aligned `ListState`: only rows in view (plus 600px of overdraw) are
  laid out and painted, and row heights are cached, so 100k messages cost about
  the same per frame as 20. Try it with `CHAOS_DEMO_MESSAGES=100000`. The
  channel list uses `uniform_list` (fixed row height, so no measuring at all).
- **Cached views.** The root wraps each region in `AnyView::cached`, so a view
  re-renders only when it is notified itself. The catch-up shimmer is the one
  thing that animates; it lives in its own cached `CatchUp` view, so each frame
  re-renders that panel (and the cheap root layout) and replays everything else
  from the previous frame.
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
  (the channel list, catch-up card and mentions take the extra height). The
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
