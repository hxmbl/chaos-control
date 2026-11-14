//! Window titlebar: sidebar toggles at either end, the centred search pill and,
//! with client-side decorations, window controls. On macOS the native traffic
//! lights sit in the top-left corner, so the left toggle moves past them.

use std::time::{Duration, Instant};

use gpui::{
    Context, Decorations, Entity, Hsla, MouseButton, Transformation, Window, WindowControlArea,
    div, prelude::*, px, radians,
};
use ui::{
    ActiveTheme, Ellipsis, IconName, Tooltip, TypeStyleExt, icon,
    tokens::{icon as icons, radius, size, space, text},
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
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const HOVER_TEXT_FADE: Duration = Duration::from_millis(200);
/// Update-check animation: spin the refresh arrow, break it into a dashed
/// ring, then land on the tick — where it stops (it never cycles past it).
const UPDATE_SPIN: Duration = Duration::from_millis(400);
const UPDATE_DASH: Duration = Duration::from_millis(800); // NOTE: should be temporary, wait till self updating is hooked
const UPDATE_POP: Duration = Duration::from_millis(300);
/// Crossfade between two glyphs of the update animation.
const UPDATE_FADE: Duration = Duration::from_millis(100);
/// Full turn in radians, for the spinning glyphs.
const TAU: f32 = std::f32::consts::TAU;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    /// Set when an update is available (not yet wired to a real updater).
    #[allow(dead_code)]
    Available,
}

pub struct Titlebar {
    panels: Entity<Panels>,
    search: Entity<Search>,
    /// Set on mouse-down so a following drag moves the window (Linux CSD).
    should_move: bool,
    app_icon_hovered: bool,
    app_icon_fade_started_at: Option<Instant>,
    app_icon_fade_from: f32,
    update_state: UpdateState,
    /// When the current check-for-updates animation started.
    update_anim_started_at: Option<Instant>,
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
            app_icon_hovered: false,
            app_icon_fade_started_at: None,
            app_icon_fade_from: 0.0,
            update_state: UpdateState::Idle,
            update_anim_started_at: None,
        }
    }

    /// Starts a check. The glyph spins, dashes, then lands on the tick and
    /// stops there — it never cycles past it.
    fn start_update_check(&mut self, cx: &mut Context<Self>) {
        if self.update_state == UpdateState::Checking {
            return;
        }
        self.update_state = UpdateState::Checking;
        self.update_anim_started_at = Some(Instant::now());
        cx.notify();
    }

    /// The update button's glyph: the refresh arrow spins, breaks into a
    /// dashed ring, then lands on the tick (or the resting state when idle).
    fn update_glyph(&self, color: Hsla) -> impl IntoElement {
        let md = icons::MD;
        let resting = div().relative().size(md);
        let Some(started) = self.update_anim_started_at else {
            let name = match self.update_state {
                UpdateState::UpToDate => IconName::Check,
                UpdateState::Available => IconName::DownloadSimple,
                UpdateState::Idle | UpdateState::Checking => IconName::ArrowClockwise,
            };
            return resting.child(icon(name, md, color));
        };

        let spin = UPDATE_SPIN.as_secs_f32();
        let dash = UPDATE_DASH.as_secs_f32();
        let pop = UPDATE_POP.as_secs_f32();
        let fade = UPDATE_FADE.as_secs_f32();
        let e = started.elapsed().as_secs_f32();
        // Crossfade progress out of / into the neighbouring glyph.
        let out = |from: f32, span: f32| ((e - from) / span).clamp(0.0, 1.0);
        let arrow_op = 1.0 - out(spin, fade);
        let dash_op = out(spin, fade) * (1.0 - out(spin + dash, fade));
        let check_op = out(spin + dash, fade);

        let mut el = resting;
        if arrow_op > 0.001 {
            // One eased turn over the spin.
            let p = (e / spin).min(1.0);
            let angle = TAU * (1.0 - (1.0 - p).powi(2));
            el = el.child(
                icon(IconName::ArrowClockwise, md, color)
                    .absolute()
                    .inset_0()
                    .opacity(arrow_op)
                    .with_transformation(Transformation::rotate(radians(angle))),
            );
        }
        if dash_op > 0.001 {
            let p = ((e - spin) / dash).clamp(0.0, 1.0);
            el = el.child(
                icon(IconName::CircleDashed, md, color)
                    .absolute()
                    .inset_0()
                    .opacity(dash_op)
                    .with_transformation(Transformation::rotate(radians(TAU * p))),
            );
        }
        if check_op > 0.001 {
            // Ease-out pop so the tick lands rather than snaps in.
            let p = out(spin + dash, pop);
            let eased = 1.0 - (1.0 - p).powi(3);
            let scale = 0.6 + 0.4 * eased;
            el = el.child(
                icon(IconName::Check, md, color)
                    .absolute()
                    .inset_0()
                    .opacity(check_op)
                    .with_transformation(Transformation::scale(gpui::size(scale, scale))),
            );
        }
        el
    }
}

impl Render for Titlebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        // The update glyph animates: keep frames coming while it runs, then
        // freeze it on the tick.
        if let Some(started) = self.update_anim_started_at {
            let total = UPDATE_SPIN + UPDATE_DASH + UPDATE_POP + UPDATE_FADE;
            if started.elapsed() < total {
                window.request_animation_frame();
            } else {
                self.update_state = UpdateState::UpToDate;
                self.update_anim_started_at = None;
            }
        }
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
                        .rounded(radius::FIELD)
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
                        .rounded(radius::FIELD)
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
            .child({
                let target = if self.app_icon_hovered { 1.0 } else { 0.0 };
                let progress = self
                    .app_icon_fade_started_at
                    .map(|at| (at.elapsed().as_secs_f32() / HOVER_TEXT_FADE.as_secs_f32()).min(1.0))
                    .unwrap_or(1.0);
                let eased = 1.0 - (1.0 - progress).powi(3);
                let hover_text_opacity =
                    self.app_icon_fade_from + (target - self.app_icon_fade_from) * eased;
                if progress < 1.0 {
                    window.request_animation_frame();
                }
                self.render_toggles(window, cx, hover_text_opacity)
            })
            .when(client_decorations, |this| {
                this.child(window_controls(window, cx))
            })
    }
}

impl Titlebar {
    /// Zed/Arc-style panel toggles: left sidebar at the left end, right sidebar
    /// at the right end. Highlighted while their panel is open.
    fn render_toggles(
        &self,
        window: &Window,
        cx: &Context<Self>,
        hover_text_opacity: f32,
    ) -> impl IntoElement {
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
        let right_tip = tip(Side::Right, "activity panel");
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
                .rounded(radius::MD)
                .when(on, |s| s.bg(selected))
                .hover(move |s| s.bg(if on { selected } else { hover }))
                // Clicks on the button must not start a window drag.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(icon(name, icons::MD, if on { t.ink } else { t.muted }))
        };

        let update_tip = match self.update_state {
            UpdateState::Idle => "Check for updates",
            UpdateState::Checking => "Checking for updates…",
            UpdateState::UpToDate => "Up to date",
            UpdateState::Available => "Download update",
        };

        let app_hovered = self.app_icon_hovered;
        let show_details = app_hovered || hover_text_opacity > 0.001;
        let app_group = div()
            .id("app-icon-group")
            .absolute()
            .top(space::X3)
            .left(left_x)
            .h(size::BUTTON_SM)
            .flex()
            .items_center()
            .gap(space::X1)
            .rounded(radius::MD)
            .on_hover(cx.listener(|this, over: &bool, _, cx| {
                let over = *over;
                if over != this.app_icon_hovered {
                    let progress = this
                        .app_icon_fade_started_at
                        .map(|at| {
                            (at.elapsed().as_secs_f32() / HOVER_TEXT_FADE.as_secs_f32()).min(1.0)
                        })
                        .unwrap_or(1.0);
                    let eased = 1.0 - (1.0 - progress).powi(3);
                    let target_before = if this.app_icon_hovered { 1.0 } else { 0.0 };
                    let current =
                        this.app_icon_fade_from + (target_before - this.app_icon_fade_from) * eased;
                    this.app_icon_fade_from = current;
                    this.app_icon_fade_started_at = Some(Instant::now());
                    this.app_icon_hovered = over;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .id("toggle-left-sidebar")
                    .size(size::BUTTON_SM)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(radius::MD)
                    .when(left_open, |s| s.bg(selected))
                    .hover(move |s| if left_open { s } else { s.bg(hover) })
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleLeftSidebar), cx)
                    })
                    .when(!app_hovered, |el| {
                        el.tooltip(Tooltip::text(tip(Side::Left, "sidebar")))
                    })
                    .child(icon(
                        IconName::AppMark,
                        icons::MD,
                        if left_open { t.ink } else { t.muted },
                    )),
            )
            .when(show_details, |group| {
                group
                    .child(
                        div()
                            .type_style(text::SMALL)
                            .font_weight(text::SEMIBOLD)
                            .text_color(t.ink_soft)
                            .whitespace_nowrap()
                            .opacity(hover_text_opacity)
                            .child(format!("Chaos Control: {APP_VERSION}")),
                    )
                    .child(
                        div()
                            .id("app-update-button")
                            .size(size::BUTTON_SM)
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius::MD)
                            .border_1()
                            .border_color(t.line)
                            .bg(t.surface)
                            .hover(move |s| s.bg(hover))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_update_check(cx);
                            }))
                            .tooltip(Tooltip::text(update_tip))
                            .opacity(hover_text_opacity)
                            .child(self.update_glyph(t.ink_soft)),
                    )
            });

        div().absolute().inset_0().child(app_group).child(
            toggle("toggle-right-sidebar", IconName::SidebarRight, right_open)
                .top(space::X3)
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
            .size(size::BUTTON_SM)
            .flex()
            .items_center()
            .justify_center()
            .rounded(radius::MD)
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
