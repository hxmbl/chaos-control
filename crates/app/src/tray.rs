//! macOS menu bar item: the mark as a template glyph, no menu. A left click
//! brings Chaos Control forward and activates its window.

use futures::{StreamExt as _, channel::mpsc};
use gpui::App;
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// 36px black-plus-alpha glyph; macOS tints template images for the menu bar.
const GLYPH: &[u8] = include_bytes!("../../../assets/app/trayTemplate@2x.png");

/// Adds the menu bar item. Call on the main thread once the app has launched.
pub fn init(cx: &mut App) {
    let tray = match build() {
        Ok(tray) => tray,
        Err(err) => {
            eprintln!("chaos-control: could not add the menu bar item: {err}");
            return;
        }
    };

    // tray-icon calls the handler on the main thread; the channel hands each
    // event to the foreground executor without polling.
    let (tx, mut rx) = mpsc::unbounded();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tx.unbounded_send(event);
    }));

    cx.spawn(async move |cx| {
        // The item disappears when the TrayIcon drops, so this task owns it
        // for the life of the app.
        let _tray = tray;
        while let Some(event) = rx.next().await {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = cx.update(bring_forward);
            }
        }
    })
    .detach();
}

fn build() -> Result<TrayIcon, Box<dyn std::error::Error>> {
    let glyph = image::load_from_memory_with_format(GLYPH, image::ImageFormat::Png)?.into_rgba8();
    let (width, height) = glyph.dimensions();
    let icon = Icon::from_rgba(glyph.into_raw(), width, height)?;
    Ok(TrayIconBuilder::new()
        .with_icon(icon)
        .with_icon_as_template(true)
        .with_tooltip("Chaos Control")
        .build()?)
}

/// The app quits when its last window closes (see main.rs), so there is always
/// a window to bring back while the menu bar item exists.
fn bring_forward(cx: &mut App) {
    cx.activate(true);
    if let Some(window) = cx.windows().first().copied() {
        let _ = window.update(cx, |_, window, _| window.activate_window());
    }
}
