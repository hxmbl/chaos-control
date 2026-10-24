//! Window titlebar: sidebar toggles at either end, the centred search pill and,
//! with client-side decorations, window controls. On macOS the native traffic
//! lights sit in the top-left corner, so the left toggle moves past them.

use gpui::{
    Context, Decorations, Entity, Focusable, MouseButton, Window, WindowControlArea, div,
    prelude::*, px,
};
use ui::{ActiveTheme, IconName, Tooltip, icon};

use crate::{
    panels::{Panels, ToggleLeftSidebar, ToggleRightSidebar},
    search::Search,
    workspace::WIDE_BREAKPOINT,
};

/// Where the left toggle sits on macOS: just right of the traffic lights.
const MAC_TRAFFIC_LIGHTS_END: f32 = 84.;
/// Width of the Linux window controls (three 28px buttons, 4px gaps) plus a gap.
const WINDOW_CONTROLS_WIDTH: f32 = 3. * 28. + 2. * 4. + 8.;

pub struct Titlebar {
    panels: Entity<Panels>,
    search: Entity<Search>,
    /// Set on mouse-down so a following drag moves the window (Linux CSD).
    should_move: bool,
}

impl Titlebar {
    pub fn new(
        panels: Entity<Panels>,
        search: Entity<Search>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&panels, |_, _, cx| cx.notify()).detach();
        Self {
            panels,
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
        let field = self
            .search
            .update(cx, |search, cx| search.field(window, cx));

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
                    .child(div().w_full().px(px(32.)).child(field)),
            )
            .child(self.render_toggles(window, cx))
            .when(client_decorations, |this| {
                this.child(window_controls(window, cx))
            })
    }
}

impl Titlebar {
    /// Zed/Arc-style panel toggles: left sidebar at the left end, right sidebar
    /// at the right end. Highlighted while their panel is open.
    fn render_toggles(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let panels = self.panels.read(cx);
        let (left_open, right_open) = (panels.left_open(), panels.right_open());
        let narrow = window.viewport_size().width < WIDE_BREAKPOINT;
        let client_decorations = matches!(window.window_decorations(), Decorations::Client { .. });
        let mac_lights = cfg!(target_os = "macos") && !window.is_fullscreen();

        let left_x = if mac_lights {
            px(MAC_TRAFFIC_LIGHTS_END)
        } else {
            px(12.)
        };
        let right_x = if client_decorations {
            px(12. + WINDOW_CONTROLS_WIDTH)
        } else {
            px(12.)
        };

        let (hover, selected) = (t.hover, t.selected);
        let toggle = |id: &'static str, name: IconName, on: bool| {
            div()
                .id(id)
                .absolute()
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .when(on, |s| s.bg(selected))
                .hover(move |s| s.bg(if on { selected } else { hover }))
                // Clicks on the button must not start a window drag.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(icon(name, px(17.), if on { t.ink } else { t.muted }))
        };

        let right_tip = match (right_open, narrow) {
            (_, true) => "Activity panel (hidden below 1200px wide)",
            (true, false) => "Hide activity panel",
            (false, false) => "Show activity panel",
        };

        div()
            .absolute()
            .inset_0()
            .child(
                toggle("toggle-left-sidebar", IconName::SidebarLeft, left_open)
                    .top(px(12.))
                    .left(left_x)
                    .tooltip(Tooltip::text(if left_open {
                        "Hide sidebar"
                    } else {
                        "Show sidebar"
                    }))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleLeftSidebar), cx)
                    }),
            )
            .child(
                toggle(
                    "toggle-right-sidebar",
                    IconName::SidebarRight,
                    right_open && !narrow,
                )
                .top(px(12.))
                .right(right_x)
                .tooltip(Tooltip::text(right_tip))
                .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleRightSidebar), cx)),
            )
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
