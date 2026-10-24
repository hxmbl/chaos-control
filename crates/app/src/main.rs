//! Chaos Control: a group chat for a project's AI agents and the people running it.

mod composer;
mod conversation;
mod panels;
mod right_sidebar;
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
    Application::new()
        .with_assets(ui::Assets)
        .run(|cx: &mut App| {
            if let Err(err) = ui::load_fonts(cx) {
                eprintln!("chaos-control: could not load bundled fonts: {err:#}");
            }
            ui::init(cx);

            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([
                KeyBinding::new("secondary-q", Quit, None),
                KeyBinding::new("secondary-b", panels::ToggleLeftSidebar, None),
                KeyBinding::new("secondary-alt-b", panels::ToggleRightSidebar, None),
            ]);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let store = cx.new(|_| store::ChatStore::new(model::sample()));
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
