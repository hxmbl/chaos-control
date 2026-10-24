//! Window titlebar: centred search pill and, with client-side decorations,
//! window controls. On macOS the native traffic lights sit in the top-left gap.

use gpui::{
    Context, Decorations, Entity, Focusable, MouseButton, Window, WindowControlArea, div,
    prelude::*, px,
};
use ui::{ActiveTheme, IconName, TextInput, icon};

pub struct Titlebar {
    search: Entity<TextInput>,
    /// Set on mouse-down so a following drag moves the window (Linux CSD).
    should_move: bool,
}

impl Titlebar {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search =
            cx.new(|cx| TextInput::new("Looking for something? Let's search.", cx).centered());
        Self {
            search,
            should_move: false,
        }
    }
}

impl Render for Titlebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        // Search is 396px wide, shrinking to 46% of the window on narrow sizes.
        let search_width = (window.viewport_size().width * 0.46).min(px(396.));
        let client_decorations = matches!(window.window_decorations(), Decorations::Client { .. });
        let hover_line = t.line_strong;
        let search_focused = self.search.focus_handle(cx).is_focused(window);

        div()
            .id("titlebar")
            .relative()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .px(px(12.))
            .window_control_area(WindowControlArea::Drag)
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
            .when(client_decorations, |this| {
                this.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.should_move = true),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.should_move = false),
                )
                .on_mouse_move(cx.listener(|this, _, window, _| {
                    if this.should_move {
                        this.should_move = false;
                        window.start_window_move();
                    }
                }))
            })
            .child(
                div()
                    .id("search")
                    .relative()
                    .flex()
                    .items_center()
                    .h(px(30.))
                    .w(search_width)
                    .rounded_full()
                    .border_1()
                    .border_color(t.line)
                    .when(search_focused, |s| s.border_color(hover_line))
                    .bg(t.surface)
                    .text_color(t.ink)
                    .text_size(px(13.))
                    .line_height(px(18.))
                    .hover(move |s| s.border_color(hover_line))
                    // Clicks on the field must not start a window drag.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().absolute().left(px(12.)).child(icon(
                        IconName::MagnifyingGlassBold,
                        px(15.),
                        t.muted,
                    )))
                    .child(div().w_full().px(px(32.)).child(self.search.clone())),
            )
            .when(client_decorations, |this| {
                this.child(window_controls(window, cx))
            })
    }
}

/// Minimise, maximise and close for client-side decorations (Linux).
fn window_controls(window: &Window, cx: &Context<Titlebar>) -> impl IntoElement {
    let t = *cx.theme();
    let hover = t.hover;
    let button = |id: &'static str, name: IconName| {
        div()
            .id(id)
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .hover(move |s| s.bg(hover))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(icon(name, px(14.), t.ink_soft))
    };
    let maximized = window.is_maximized();
    div()
        .absolute()
        .right(px(12.))
        .flex()
        .gap(px(4.))
        .child(
            button("minimize", IconName::Minus).on_click(|_, window, _| window.minimize_window()),
        )
        .child(
            button(
                "maximize",
                if maximized {
                    IconName::CopySimple
                } else {
                    IconName::Square
                },
            )
            .on_click(|_, window, _| window.zoom_window()),
        )
        .child(button("close", IconName::X).on_click(|_, window, _| window.remove_window()))
}
