//! Chaos Control: a group chat for a project's AI agents and the people running it.

mod agents;
mod composer;
mod conversation;
mod pane_host;
mod panels;
mod project;
mod right_sidebar;
mod search;
mod settings;
mod sidebar;
mod store;
mod titlebar;
mod workspace;

use gpui::{
    App, Application, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowDecorations,
    WindowOptions, actions, point, prelude::*, px, size,
};

actions!(chaos_control, [Quit]);

fn main() {
    // `chaos-control mcp-bridge`: the stdio MCP server agents start when they
    // can't take our chat tools over HTTP. URL and token arrive in the env.
    if std::env::args().nth(1).as_deref() == Some("mcp-bridge") {
        if let Err(err) = chaos_core::mcp::bridge_main() {
            eprintln!("chaos-control mcp-bridge: {err}");
            std::process::exit(1);
        }
        return;
    }
    // The agent runtime, when an agents.json exists (see agents.rs).
    let core = agents::start();

    Application::new()
        .with_assets(ui::Assets)
        .run(|cx: &mut App| {
            if let Err(err) = ui::load_fonts(cx) {
                eprintln!("chaos-control: could not load bundled fonts: {err:#}");
            }
            ui::init(cx);
            settings::init(cx);

            cx.on_action(|_: &Quit, cx| cx.quit());
            // Shortcuts stay conventional (see "Shortcuts" in ARCHITECTURE.md): a
            // combo people already know keeps its usual meaning here.
            cx.bind_keys([
                KeyBinding::new("secondary-q", Quit, None),
                // Plain cmd/ctrl-[ and ] stay free for back/forward through
                // channel history; cmd/ctrl-B stays free for bold in the composer.
                KeyBinding::new("secondary-alt-[", panels::ToggleLeftSidebar, None),
                KeyBinding::new("secondary-alt-]", panels::ToggleRightSidebar, None),
                // TODO: cmd/ctrl-F and ctrl-shift-P will focus the bar too once it
                // doubles as the command palette. Reserved, not bound yet.
                KeyBinding::new("secondary-k", search::FocusSearch, None),
                KeyBinding::new("down", search::SelectNext, Some(search::KEY_CONTEXT)),
                KeyBinding::new("up", search::SelectPrevious, Some(search::KEY_CONTEXT)),
                KeyBinding::new("escape", search::Dismiss, Some(search::KEY_CONTEXT)),
            ]);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let store = cx.new(|_| store::ChatStore::new(model::sample()));
            match core {
                Some(link) => store.update(cx, |store, cx| store.attach_core(link, cx)),
                None => store.update(cx, |store, cx| store.load_sample_history(cx)),
            }
            if let Some(count) = demo_message_count() {
                store.update(cx, |store, cx| store.load_demo_history(count, cx));
            }

            let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(760.), px(520.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Chaos Control".into()),
                    // macOS: draw our own titlebar under the native traffic lights,
                    // vertically centred on the 52px bar.
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(18.), px(19.))),
                }),
                // Linux: ask for client-side decorations so the titlebar is ours there too.
                // The compositor may still insist on server decorations; the view adapts.
                window_decorations: Some(WindowDecorations::Client),
                app_id: Some("chaos-control".into()),
                ..Default::default()
            };

            let opened = cx.open_window(options, |window, cx| {
                cx.new(|cx| workspace::Workspace::new(store.clone(), window, cx))
            });
            match opened {
                Ok(_) => cx.activate(true),
                Err(err) => {
                    eprintln!("chaos-control: could not open a window: {err:#}");
                    cx.quit();
                }
            }
        });
}

/// `CHAOS_DEMO_MESSAGES=100000` fills #main with synthetic history, for checking
/// that scrolling stays smooth with very large channels.
fn demo_message_count() -> Option<usize> {
    std::env::var("CHAOS_DEMO_MESSAGES")
        .ok()?
        .parse()
        .ok()
        .filter(|n| *n > 0)
}
