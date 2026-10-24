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
  few components the shell needs (pill, icon, tooltip, text input). Hand-built
  to match the design rather than pulling in a component kit.
- `app`: the binary. One view per region: `Titlebar`, `Sidebar`,
  `RightSidebar` (+ `CatchUp`), `Conversation`, `Composer`, laid out by the
  root `Workspace` view.

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
already laid out is left alone. When a backend exists, it receives on a
background task and hands finished messages to `append` on the main thread,
batched per frame when many agents post at once.

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
  macOS traffic lights) and `cmd/ctrl-B` / `cmd/ctrl-alt-B`. The open state is
  user intent held in an `Entity<Panels>`; toggling slides the panel off its
  edge over 180ms while the centre column reflows. Below 1200px of width the
  right sidebar stays hidden whatever its toggle says, and comes back on
  widening only if it was left open.
- Layout: both sidebars float 12px from the window edges and run from just
  under the titlebar to 12px above the bottom, so they grow with the window
  (the channel list, catch-up card and mentions take the extra height). The
  conversation and composer share one centred column that widens with the
  window, from 720px up to 960px.
