//! Root view: lays out the titlebar, both floating sidebars, the conversation and
//! the composer, and keeps the theme in step with the OS appearance.

use gpui::{
    AnyView, Context, CursorStyle, Decorations, Entity, MouseButton, Pixels, ResizeEdge,
    StyleRefinement, Subscription, Window, div, prelude::*, px,
};
use ui::{ActiveTheme, FONT_FAMILY, Theme};

use crate::{
    composer::Composer, conversation::Conversation, right_sidebar::RightSidebar, sidebar::Sidebar,
    store::ChatStore, titlebar::Titlebar,
};

/// Below this window width only the left sidebar shows.
const WIDE_BREAKPOINT: Pixels = px(1200.);
const LEFT_WIDTH: Pixels = px(258.);
const RIGHT_WIDTH: Pixels = px(320.);
const SIDE_INSET: Pixels = px(12.);
pub const TITLEBAR_HEIGHT: Pixels = px(52.);

pub struct Workspace {
    titlebar: Entity<Titlebar>,
    sidebar: Entity<Sidebar>,
    right_sidebar: Entity<RightSidebar>,
    conversation: Entity<Conversation>,
    composer: Entity<Composer>,
    _appearance: Subscription,
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

        Self {
            titlebar: cx.new(|cx| Titlebar::new(window, cx)),
            sidebar: cx.new(|cx| Sidebar::new(store.clone(), cx)),
            right_sidebar: cx.new(|cx| RightSidebar::new(store.clone(), cx)),
            conversation: cx.new(|cx| Conversation::new(store.clone(), cx)),
            composer: cx.new(|cx| Composer::new(store.clone(), window, cx)),
            _appearance: appearance,
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
        let wide = viewport.width >= WIDE_BREAKPOINT;

        // The sidebars float with the wireframe's proportions: ~14% of the window
        // height above and below, clamped so short windows keep a margin.
        let side_top = (viewport.height * 0.143 - TITLEBAR_HEIGHT).max(px(4.));
        let side_bottom = (viewport.height * 0.143).max(px(12.));

        // Centre column. Once the right sidebar shows, both sides reserve the wider
        // panel's width so search and composer stay mid-window.
        let (pad_l, pad_r) = if wide {
            (px(344.), px(344.))
        } else {
            (px(290.), px(20.))
        };

        let decorations = window.window_decorations();

        div()
            .id("workspace")
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
                            .pb(px(128.))
                            .child(cached(&self.conversation)),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom(px(24.))
                            .pl(pad_l)
                            .pr(pad_r)
                            .flex()
                            .justify_center()
                            .child(div().w_full().max_w(px(800.)).child(self.composer.clone())),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(SIDE_INSET)
                            .w(LEFT_WIDTH)
                            .top(side_top)
                            .bottom(side_bottom)
                            .child(cached(&self.sidebar)),
                    )
                    .when(wide, |this| {
                        this.child(
                            div()
                                .absolute()
                                .right(SIDE_INSET)
                                .w(RIGHT_WIDTH)
                                .top(side_top)
                                .bottom(side_bottom)
                                .child(cached(&self.right_sidebar)),
                        )
                    }),
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
