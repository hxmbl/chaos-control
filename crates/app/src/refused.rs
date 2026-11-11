//! The window shown instead of the workspace when core refuses its data (a
//! newer build wrote it): the reason, and nothing that could touch the data.

use gpui::{
    App, Bounds, Context, SharedString, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    size,
};
use ui::{
    ActiveTheme, FONT_FAMILY, TypeStyleExt,
    tokens::{space, text},
};

struct Refused {
    message: SharedString,
}

impl Render for Refused {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(space::X2)
            .p(space::X8)
            .bg(t.canvas)
            .font_family(FONT_FAMILY)
            .text_color(t.ink)
            .child(
                div()
                    .type_style(text::TITLE)
                    .child("Chaos Control can't open its data"),
            )
            .child(
                div()
                    .max_w(px(480.))
                    .type_style(text::BODY)
                    .text_color(t.ink_soft)
                    .child(self.message.clone()),
            )
    }
}

/// Opens the refusal window. Closing it quits (see `main`).
pub fn open(message: SharedString, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(560.), px(240.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        app_id: Some("chaos-control".into()),
        ..Default::default()
    };
    match cx.open_window(options, |_, cx| cx.new(|_| Refused { message })) {
        Ok(_) => cx.activate(true),
        Err(err) => {
            eprintln!("chaos-control: could not open a window: {err:#}");
            cx.quit();
        }
    }
}
