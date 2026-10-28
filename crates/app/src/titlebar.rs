//! Window titlebar: sidebar toggles at either end, the centred search pill and,
//! with client-side decorations, window controls. On macOS the native traffic
//! lights sit in the top-left corner, so the left toggle moves past them.

use gpui::{
    Context, Decorations, Entity, MouseButton, Window, WindowControlArea, div, prelude::*, px,
};
use ui::{
    ActiveTheme, Ellipsis, IconName, Tooltip, TypeStyleExt, icon,
    tokens::{icon as icons, size, space, text},
    truncated,
};

use crate::{
    panels::{Mode, Panels, Side, ToggleLeftSidebar, ToggleRightSidebar},
    search::{self, Search},
};

/// Where the left toggle sits on macOS: just right of the traffic lights.
const MAC_TRAFFIC_LIGHTS_END: f32 = 84.;
/// Width of the Linux window controls (three 28px buttons, 4px gaps) plus a gap.
const WINDOW_CONTROLS_WIDTH: f32 = 3. * 28. + 2. * 4. + 8.; // 3 × size::BUTTON_SM, 2 × space::X1, space::X2

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
        // The pill hides while search is open (it has grown into the panel).
        cx.observe(&search, |_, _, cx| cx.notify()).detach();
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
        let search_width = search::pill_width(window.viewport_size().width);
        let search_open = self.search.read(cx).is_open();
        let search_in_pane = Search::in_pane(cx);
        let client_decorations = matches!(window.window_decorations(), Decorations::Client { .. });
        let hover_line = t.line_strong;
        let field = (!search_open).then(|| {
            self.search
                .update(cx, |search, cx| search.field(window, cx))
        });

        div()
            .id("titlebar")
            .relative()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .px(space::X3)
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
            // While search is open the workspace draws the grown panel over
            // this spot; an empty box keeps the titlebar layout unchanged.
            .when(search_open && !search_in_pane, |this| {
                this.child(div().h(search::PILL_HEIGHT).w(search_width))
            })
            // Search in the pane: the field lives in the pane, so the pill
            // stays put as a quiet stand-in showing the query.
            .when(search_open && search_in_pane, |this| {
                let query = self.search.read(cx).query(cx);
                let empty = query.is_empty();
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(space::X2)
                        .h(search::PILL_HEIGHT)
                        .w(search_width)
                        .px(space::X3)
                        .rounded_full()
                        .bg(t.sunken)
                        .type_style(text::SMALL)
                        .text_color(t.muted)
                        .child(icon(IconName::MagnifyingGlass, icons::MD, t.muted))
                        .child(div().min_w_0().ellipsis().child(truncated(if empty {
                            "Searching in the pane".into()
                        } else {
                            query
                        }))),
                )
            })
            .when_some(field, |this, field| {
                this.child(
                    div()
                        .id("search")
                        .relative()
                        .flex()
                        .items_center()
                        .h(search::PILL_HEIGHT)
                        .w(search_width)
                        .rounded_full()
                        .border_1()
                        .border_color(t.line)
                        .bg(t.surface)
                        .text_color(t.ink)
                        .type_style(text::SMALL)
                        .hover(move |s| s.border_color(hover_line))
                        // A click anywhere on the pill grows it into the search
                        // panel. It must not start a window drag either.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.search.update(cx, |s, cx| s.focus(window, cx));
                            }),
                        )
                        .child(div().absolute().left(space::X3).child(icon(
                            IconName::MagnifyingGlass,
                            icons::MD,
                            t.muted,
                        )))
                        .child(div().w_full().px(space::X8).child(field)),
                )
            })
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
        let (left_open, right_open) = (panels.docked(Side::Left), panels.docked(Side::Right));
        let tip = |side: Side, name: &str| -> String {
            match (panels.docked(side), panels.mode(side)) {
                (true, _) => format!("Hide {name}"),
                (false, Mode::Collapsed) => format!("Show {name}"),
                // Collapsed by the window width: showing it pins it open.
                (false, _) => format!("Pin {name} open (collapsed to fit the window)"),
            }
        };
        let (left_tip, right_tip) = (
            tip(Side::Left, "sidebar"),
            tip(Side::Right, "activity panel"),
        );
        let client_decorations = matches!(window.window_decorations(), Decorations::Client { .. });
        let mac_lights = cfg!(target_os = "macos") && !window.is_fullscreen();

        let left_x = if mac_lights {
            px(MAC_TRAFFIC_LIGHTS_END)
        } else {
            space::X3
        };
        let right_x = if client_decorations {
            space::X3 + px(WINDOW_CONTROLS_WIDTH)
        } else {
            space::X3
        };

        let (hover, selected) = (t.hover, t.selected);
        let toggle = |id: &'static str, name: IconName, on: bool| {
            div()
                .id(id)
                .absolute()
                .size(size::BUTTON_SM)
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .when(on, |s| s.bg(selected))
                .hover(move |s| s.bg(if on { selected } else { hover }))
                // Clicks on the button must not start a window drag.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(icon(name, icons::MD, if on { t.ink } else { t.muted }))
        };

        div()
            .absolute()
            .inset_0()
            .child(
                toggle("toggle-left-sidebar", IconName::SidebarLeft, left_open)
                    .top(space::X3)
                    .left(left_x)
                    .tooltip(Tooltip::text(left_tip))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleLeftSidebar), cx)
                    }),
            )
            .child(
                toggle("toggle-right-sidebar", IconName::SidebarRight, right_open)
                    .top(space::X3)
                    .right(right_x)
                    .tooltip(Tooltip::text(right_tip))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleRightSidebar), cx)
                    }),
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
            .size(size::BUTTON_SM)
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .hover(move |s| s.bg(hover))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(icon(name, icons::SM, t.ink_soft))
    };
    let maximized = window.is_maximized();
    div()
        .absolute()
        .right(space::X3)
        .flex()
        .gap(space::X1)
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
