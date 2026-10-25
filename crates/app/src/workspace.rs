//! Root view: lays out the titlebar, both floating sidebars, the conversation and
//! the composer, and keeps the theme in step with the OS appearance.

use std::time::Instant;

use gpui::{
    AnyView, Context, CursorStyle, Decorations, DispatchPhase, Entity, FocusHandle, MouseButton,
    MouseExitEvent, MouseMoveEvent, Pixels, ResizeEdge, StyleRefinement, Subscription, Window,
    canvas, div, prelude::*, px,
};
use ui::{ActiveTheme, FONT_FAMILY, Theme};

use crate::{
    composer::Composer,
    conversation::Conversation,
    panels::{
        GUTTER, LEFT_WIDTH, Panels, RIGHT_WIDTH, SIDE_INSET, Side, SideFrame, ToggleLeftSidebar,
        ToggleRightSidebar,
    },
    right_sidebar::RightSidebar,
    search::{FocusSearch, Search, SearchEvent},
    sidebar::Sidebar,
    store::ChatStore,
    titlebar::Titlebar,
};

/// The sidebars start just under the titlebar.
const SIDE_TOP: Pixels = px(4.);
/// Corner radius of both sidebar panels (their views draw the same).
const SIDE_RADIUS: Pixels = px(26.);
/// The conversation and composer widen with the window between these widths.
const COLUMN_MIN: Pixels = px(720.);
const COLUMN_MAX: Pixels = px(960.);
/// Height kept clear under the conversation for the floating composer.
const COMPOSER_CLEARANCE: Pixels = px(112.);
pub const TITLEBAR_HEIGHT: Pixels = px(52.);

/// Where the centre column goes: left and right padding plus its maximum width.
///
/// `left`/`right` are the widths the sidebars take from each edge (zero when
/// hidden). When there's room, both sides reserve the larger of the two so the
/// conversation and composer stay centred under the titlebar search; when that
/// would squeeze the column, it centres in the space actually left over.
fn centre_column(viewport: Pixels, left: Pixels, right: Pixels) -> (Pixels, Pixels, Pixels) {
    let max_w = (viewport * 0.6).max(COLUMN_MIN).min(COLUMN_MAX);
    let (l, r) = (left + GUTTER, right + GUTTER);
    let side = l.max(r);
    let fits_centred = viewport - side - side >= max_w.min(viewport - l - r);
    if fits_centred {
        (side, side, max_w)
    } else {
        (l, r, max_w)
    }
}

pub struct Workspace {
    panels: Entity<Panels>,
    search: Entity<Search>,
    titlebar: Entity<Titlebar>,
    sidebar: Entity<Sidebar>,
    right_sidebar: Entity<RightSidebar>,
    conversation: Entity<Conversation>,
    composer: Entity<Composer>,
    /// Tracks keyboard focus anywhere inside each sidebar, so a peek holds
    /// open while the user is typing or tabbing in it.
    side_focus: [FocusHandle; 2],
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(store: Entity<ChatStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            cx.set_global(Theme::for_appearance(window.appearance()));
            // Theme is read during render, so every view needs to render again.
            window.refresh();
        });
        // The app-level theme was picked before the window existed; trust the window.
        cx.set_global(Theme::for_appearance(window.appearance()));

        let panels = cx.new(|_| Panels::new());
        let viewport = window.viewport_size();
        panels.update(cx, |p, _| p.layout(viewport, Instant::now()));
        let panels_changed = cx.observe(&panels, |_, _, cx| cx.notify());
        // Re-apply the collapse rules whenever the window is resized.
        let resized = cx.observe_window_bounds(window, |this, window, cx| {
            let viewport = window.viewport_size();
            this.panels.update(cx, |p, cx| {
                if p.layout(viewport, Instant::now()) {
                    cx.notify();
                }
            });
        });
        let side_focus = [cx.focus_handle(), cx.focus_handle()];
        let mut focus_subscriptions = Vec::new();
        for side in [Side::Left, Side::Right] {
            let handle = &side_focus[side as usize];
            focus_subscriptions.push(cx.on_focus_in(handle, window, move |this, _, cx| {
                this.panels
                    .update(cx, |p, _| p.set_held(side, true, Instant::now()));
            }));
            focus_subscriptions.push(cx.on_focus_out(handle, window, move |this, _, _, cx| {
                this.panels.update(cx, |p, cx| {
                    p.set_held(side, false, Instant::now());
                    p.schedule_poll(cx);
                });
            }));
        }
        let search = cx.new(|cx| {
            Search::new(
                store.clone(),
                std::sync::Arc::new(model::SubstringSearch),
                window,
                cx,
            )
        });
        let composer = cx.new(|cx| Composer::new(store.clone(), window, cx));
        // After a jump, hand the keyboard back to the composer.
        let jumped = cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
            SearchEvent::Jumped => this.composer.update(cx, |c, cx| c.focus(window, cx)),
        });

        Self {
            titlebar: cx.new(|cx| Titlebar::new(panels.clone(), search.clone(), window, cx)),
            panels,
            search,
            sidebar: cx.new(|cx| Sidebar::new(store.clone(), cx)),
            right_sidebar: cx.new(|cx| RightSidebar::new(store.clone(), cx)),
            conversation: cx.new(|cx| Conversation::new(store.clone(), cx)),
            composer,
            side_focus,
            _subscriptions: [appearance, panels_changed, resized, jumped]
                .into_iter()
                .chain(focus_subscriptions)
                .collect(),
        }
    }
}

/// Wraps a child view so it only re-renders when it is notified itself, not every
/// time something else in the window changes (an animation, a hover elsewhere).
fn cached<V: 'static + gpui::Render>(view: &Entity<V>) -> AnyView {
    AnyView::from(view.clone()).cached(StyleRefinement::default().size_full())
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let viewport = window.viewport_size();

        // Docked, collapsed or peeking, and how far through a slide.
        let ([left, right], sliding) = self.panels.read(cx).frame(Instant::now());
        if sliding {
            window.request_animation_frame();
        }

        let left_space = (SIDE_INSET + LEFT_WIDTH) * left.reserve;
        let right_space = (RIGHT_WIDTH + SIDE_INSET) * right.reserve;
        let (pad_l, pad_r, column_w) = centre_column(viewport.width, left_space, right_space);

        let decorations = window.window_decorations();

        div()
            .id("workspace")
            .key_context("Workspace")
            .on_action(cx.listener(|this, _: &ToggleLeftSidebar, _, cx| {
                this.panels.update(cx, |p, cx| {
                    p.toggle(Side::Left, Instant::now());
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ToggleRightSidebar, _, cx| {
                this.panels.update(cx, |p, cx| {
                    p.toggle(Side::Right, Instant::now());
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.search.update(cx, |s, cx| s.focus(window, cx));
            }))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(t.canvas)
            .font_family(FONT_FAMILY)
            .text_size(px(15.))
            .line_height(px(22.))
            .text_color(t.ink)
            .child(
                div()
                    .h(TITLEBAR_HEIGHT)
                    .flex_none()
                    .child(cached(&self.titlebar)),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .pl(pad_l)
                            .pr(pad_r)
                            .pb(COMPOSER_CLEARANCE)
                            .flex()
                            .justify_center()
                            .child(
                                div()
                                    .h_full()
                                    .w_full()
                                    .max_w(column_w)
                                    .child(cached(&self.conversation)),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom(SIDE_INSET)
                            .pl(pad_l)
                            .pr(pad_r)
                            .flex()
                            .justify_center()
                            .child(div().w_full().max_w(column_w).child(self.composer.clone())),
                    )
                    // Collapsed sidebars slide off their edge and fade; a peek
                    // slides one back over the content.
                    .when(left.shown > 0., |this| {
                        this.child(self.side_slot(Side::Left, left, cached(&self.sidebar), cx))
                    })
                    .when(right.shown > 0., |this| {
                        this.child(self.side_slot(
                            Side::Right,
                            right,
                            cached(&self.right_sidebar),
                            cx,
                        ))
                    }),
            )
            // Edge hover zones: watch every pointer move (capture phase, so
            // nothing on top can swallow it) and let `Panels` decide.
            .child(self.pointer_watcher(cx))
            // Search results float over everything, just under the titlebar.
            .child(
                div()
                    .absolute()
                    .top(TITLEBAR_HEIGHT - px(8.))
                    .left_0()
                    .right_0()
                    .pl(pad_l)
                    .pr(pad_r)
                    .flex()
                    .justify_center()
                    .child(div().w_full().max_w(column_w).child(self.search.clone())),
            )
            .map(|this| match decorations {
                Decorations::Server => this,
                Decorations::Client { tiling } => {
                    let this = this.when(!tiling.is_tiled(), |this| {
                        this.border_1().border_color(t.line_strong)
                    });
                    if tiling.is_tiled() || window.is_maximized() {
                        this
                    } else {
                        this.children(resize_handles())
                    }
                }
            })
    }
}

impl Workspace {
    /// One sidebar, docked or peeking. A peek floats over the chat with a
    /// deeper shadow and swallows clicks so nothing underneath reacts.
    fn side_slot(
        &self,
        side: Side,
        frame: SideFrame,
        view: AnyView,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let t = *cx.theme();
        let offset = SIDE_INSET - (side.width() + SIDE_INSET) * (1. - frame.shown);
        div()
            .id(match side {
                Side::Left => "left-sidebar",
                Side::Right => "right-sidebar",
            })
            .absolute()
            .map(|d| match side {
                Side::Left => d.left(offset),
                Side::Right => d.right(offset),
            })
            .w(side.width())
            .top(SIDE_TOP)
            .bottom(SIDE_INSET)
            .opacity(frame.shown)
            .track_focus(&self.side_focus[side as usize])
            // Clicking a sidebar must not move keyboard focus to the panel
            // itself (the composer keeps it); fields inside still focus
            // themselves. Runs after the children's handlers, before the
            // panel's own focus transfer.
            .on_any_mouse_down(|_, window, _| window.prevent_default())
            .when(frame.overlay, |d| {
                d.occlude().rounded(SIDE_RADIUS).shadow(t.overlay_shadow())
            })
            .child(view)
    }

    /// An invisible full-window canvas that feeds pointer moves and window
    /// exits to `Panels` for the edge peek.
    fn pointer_watcher(&self, _: &Context<Self>) -> impl IntoElement {
        let panels = self.panels.clone();
        canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let on_move = panels.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    on_move.update(cx, |p, cx| {
                        let opened =
                            p.pointer(Some(e.position), e.pressed_button.is_some(), Instant::now());
                        p.schedule_poll(cx);
                        if opened {
                            cx.notify();
                        }
                    });
                });
                let on_exit = panels.clone();
                window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    on_exit.update(cx, |p, cx| {
                        p.pointer(None, false, Instant::now());
                        p.schedule_poll(cx);
                    });
                });
            },
        )
        .absolute()
        .size_full()
    }
}

/// Client-side decorations (Linux): thin strips along each edge and corner that
/// start a compositor-driven resize.
fn resize_handles() -> impl Iterator<Item = gpui::AnyElement> {
    const EDGE: Pixels = px(5.);
    const CORNER: Pixels = px(12.);
    let handle = |edge: ResizeEdge| {
        let cursor = match edge {
            ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
            ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
            ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
            ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
        };
        let el = div().absolute().cursor(cursor).occlude();
        let el = match edge {
            ResizeEdge::Top => el.top_0().left(CORNER).right(CORNER).h(EDGE),
            ResizeEdge::Bottom => el.bottom_0().left(CORNER).right(CORNER).h(EDGE),
            ResizeEdge::Left => el.left_0().top(CORNER).bottom(CORNER).w(EDGE),
            ResizeEdge::Right => el.right_0().top(CORNER).bottom(CORNER).w(EDGE),
            ResizeEdge::TopLeft => el.top_0().left_0().size(CORNER),
            ResizeEdge::TopRight => el.top_0().right_0().size(CORNER),
            ResizeEdge::BottomLeft => el.bottom_0().left_0().size(CORNER),
            ResizeEdge::BottomRight => el.bottom_0().right_0().size(CORNER),
        };
        el.on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            window.start_window_resize(edge);
        })
        .into_any_element()
    };
    [
        ResizeEdge::Top,
        ResizeEdge::Bottom,
        ResizeEdge::Left,
        ResizeEdge::Right,
        ResizeEdge::TopLeft,
        ResizeEdge::TopRight,
        ResizeEdge::BottomLeft,
        ResizeEdge::BottomRight,
    ]
    .into_iter()
    .map(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_centres_when_there_is_room() {
        let (l, r, w) = centre_column(px(1920.), px(270.), px(332.));
        assert_eq!(l, r);
        assert_eq!(w, COLUMN_MAX);
    }

    #[test]
    fn column_uses_leftover_space_when_centring_would_squeeze_it() {
        let (l, r, w) = centre_column(px(1440.), px(270.), px(332.));
        assert!(l < r);
        assert!(px(1440.) - l - r <= w);
    }

    #[test]
    fn column_widens_with_the_window_within_bounds() {
        assert_eq!(centre_column(px(800.), px(0.), px(0.)).2, COLUMN_MIN);
        assert_eq!(centre_column(px(3000.), px(0.), px(0.)).2, COLUMN_MAX);
    }
}
