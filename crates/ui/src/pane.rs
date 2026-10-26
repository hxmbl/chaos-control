//! The pane: a fixed glass overlay that shows what you need or what you
//! clicked on without taking you away from what is behind it.
//!
//! Two halves:
//!
//! - [`PaneState`]: plain open/close/focus bookkeeping with no window, so it is
//!   unit-tested. It remembers where keyboard focus was when the pane opened,
//!   hands it back on close, and times the open animation.
//! - [`Pane`]: the element. A red close dot (top left, macOS-like, with a hover
//!   glyph), an optional large title or a custom header, a scrollable body that
//!   takes any element, and optional full-width quick-action rows under a thin
//!   divider. It closes on the dot, on Escape and on a click outside, and it
//!   never moves: it sits where its host puts it.
//!
//! The look comes from [`crate::Glass`] (see `glass.rs` for how the glass is
//! faked without a backdrop blur).

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, ElementId, FocusHandle, FontWeight, IntoElement, KeyBinding, Pixels,
    SharedString, Window, actions, div, prelude::*, px, rgb, svg,
};

use crate::{ActiveTheme, Ellipsis, IconName, truncated};

actions!(pane, [ClosePane]);

/// Key context on the pane (Escape closes it).
pub const KEY_CONTEXT: &str = "Pane";
/// The fade/scale-in, matching the search grow and the sidebar peek.
pub const OPEN_DURATION: Duration = Duration::from_millis(140);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", ClosePane, Some(KEY_CONTEXT))]);
}

/// Open/close state for one pane slot, generic over what it shows (`C`) and
/// the focus token to hand back (`F`, a `FocusHandle` in the app).
#[derive(Debug)]
pub struct PaneState<C, F> {
    open: Option<Opened<C, F>>,
    /// Bumped on every open or content swap, so the body's scroll position
    /// (keyed by element id) starts at the top for new content.
    generation: u64,
}

#[derive(Debug)]
struct Opened<C, F> {
    content: C,
    return_focus: Option<F>,
    opened_at: Instant,
}

impl<C, F> Default for PaneState<C, F> {
    fn default() -> Self {
        Self {
            open: None,
            generation: 0,
        }
    }
}

impl<C, F> PaneState<C, F> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn content(&self) -> Option<&C> {
        self.open.as_ref().map(|o| &o.content)
    }

    pub fn content_mut(&mut self) -> Option<&mut C> {
        self.open.as_mut().map(|o| &mut o.content)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Shows `content`. `focused` is whatever had the keyboard just before.
    ///
    /// Opening over an open pane swaps the content in place: no second
    /// animation, and focus still returns to where it was before the first
    /// pane, not to the pane being replaced.
    pub fn open(&mut self, content: C, focused: Option<F>, now: Instant) {
        self.generation += 1;
        match &mut self.open {
            Some(open) => open.content = content,
            None => {
                self.open = Some(Opened {
                    content,
                    return_focus: focused,
                    opened_at: now,
                })
            }
        }
    }

    /// Closes the pane. `None` when it was already closed; otherwise the
    /// focus to restore (itself `None` when nothing had focus before).
    pub fn close(&mut self) -> Option<Option<F>> {
        self.open.take().map(|o| o.return_focus)
    }

    /// 0..1 through the open animation, eased out (cubic). 1 when settled.
    pub fn progress(&self, now: Instant) -> f32 {
        let Some(open) = &self.open else {
            return 0.;
        };
        let t = (now.saturating_duration_since(open.opened_at).as_secs_f32()
            / OPEN_DURATION.as_secs_f32())
        .min(1.);
        1. - (1. - t).powi(3)
    }

    /// Whether the open animation still needs frames.
    pub fn animating(&self, now: Instant) -> bool {
        self.is_open() && self.progress(now) < 1.
    }
}

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// A full-width row along the bottom of the pane.
pub struct QuickAction {
    id: ElementId,
    label: SharedString,
    on_click: Handler,
}

impl QuickAction {
    pub fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            on_click: Rc::new(on_click),
        }
    }
}

/// The pane element. Its host owns a [`PaneState`] (and usually a `FocusHandle`), and
/// puts this in an absolutely positioned box covering the area the pane may
/// use (between the sidebars, under the titlebar, above the composer). The
/// pane centres itself in that box; the optional scrim covers all of it.
#[derive(IntoElement)]
pub struct Pane {
    id: ElementId,
    focus: Option<FocusHandle>,
    progress: f32,
    max_width: Pixels,
    scrim: bool,
    anchor_top: bool,
    title: Option<SharedString>,
    header: Option<AnyElement>,
    body: Vec<AnyElement>,
    actions: Vec<QuickAction>,
    on_close: Option<Handler>,
}

impl Pane {
    /// `id` should change with the content (see [`PaneState::generation`]) so
    /// the body's scroll position resets.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            focus: None,
            progress: 1.,
            max_width: px(960.),
            scrim: true,
            anchor_top: false,
            title: None,
            header: None,
            body: Vec::new(),
            actions: Vec::new(),
            on_close: None,
        }
    }

    /// The handle the pane tracks, so Escape reaches it and focus can come
    /// back. Leave it off when something inside keeps the keyboard (search's
    /// field), or a click in the pane would take focus from it.
    pub fn focus(mut self, focus: &FocusHandle) -> Self {
        self.focus = Some(focus.clone());
        self
    }

    /// 0..1 open progress (from [`PaneState::progress`]): fades and scales in.
    pub fn progress(mut self, progress: f32) -> Self {
        self.progress = progress.clamp(0., 1.);
        self
    }

    pub fn max_width(mut self, width: Pixels) -> Self {
        self.max_width = width;
        self
    }

    /// Dim the content behind the pane (on by default). The scrim stands in
    /// for the blur gpui 0.2.2 can't do.
    pub fn scrim(mut self, scrim: bool) -> Self {
        self.scrim = scrim;
        self
    }

    /// Sit at the top of the area instead of centring in it, so a pane whose
    /// height changes while it's open (search results) grows downwards
    /// instead of jumping.
    pub fn anchor_top(mut self) -> Self {
        self.anchor_top = true;
        self
    }

    pub fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Something other than a title next to the close dot (search puts its
    /// field here). Replaces the title.
    pub fn header(mut self, header: impl IntoElement) -> Self {
        self.header = Some(header.into_any_element());
        self
    }

    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.body.push(child.into_any_element());
        self
    }

    pub fn children(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.body
            .extend(children.into_iter().map(|c| c.into_any_element()));
        self
    }

    pub fn action(mut self, action: QuickAction) -> Self {
        self.actions.push(action);
        self
    }

    pub fn actions(mut self, actions: impl IntoIterator<Item = QuickAction>) -> Self {
        self.actions.extend(actions);
        self
    }

    /// Called for the close dot, Escape and a click outside.
    pub fn on_close(mut self, on_close: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(on_close));
        self
    }
}

/// macOS close-button red, its rim, and the glyph shown on hover.
const CLOSE_RED: u32 = 0xff5f57;
const CLOSE_RIM: u32 = 0xe2463f;
const CLOSE_GLYPH: u32 = 0x4d0000;
const RADIUS: Pixels = px(22.);

impl RenderOnce for Pane {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = *cx.theme();
        let glass = t.glass();
        let p = self.progress;
        let on_close = self.on_close.clone();
        let close = move |window: &mut Window, cx: &mut App| {
            if let Some(f) = &on_close {
                f(window, cx)
            }
        };
        let close_out = close.clone();
        let close_key = close.clone();
        let close_dot = close.clone();

        let group: SharedString = "pane-close".into();
        let dot = div()
            .id("pane-close")
            .group(group.clone())
            .size(px(14.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_1()
            .border_color(rgb(CLOSE_RIM))
            .bg(rgb(CLOSE_RED))
            .cursor_pointer()
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                close_dot(window, cx)
            })
            .child(
                div().invisible().group_hover(group, |s| s.visible()).child(
                    svg()
                        .path(IconName::X.path())
                        .size(px(9.))
                        .text_color(rgb(CLOSE_GLYPH)),
                ),
            );

        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .min_w_0()
            .child(dot)
            .map(|d| match (self.header, self.title) {
                (Some(header), _) => d.child(div().flex_1().min_w_0().child(header)),
                (None, Some(title)) => d.child(
                    div()
                        .flex_1()
                        .ellipsis()
                        .text_size(px(21.))
                        .line_height(px(28.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.ink)
                        .child(truncated(title)),
                ),
                (None, None) => d,
            });

        let has_actions = !self.actions.is_empty();
        let rows = self.actions.into_iter().map(|a| {
            let on_click = a.on_click.clone();
            div()
                .id(a.id)
                .flex()
                .flex_none()
                .items_center()
                .w_full()
                .h(px(36.))
                .px(px(14.))
                .rounded(px(12.))
                .bg(glass.well)
                .hover(|s| s.bg(glass.well_hover))
                .cursor_pointer()
                .text_size(px(14.))
                .text_color(t.ink)
                .on_click(move |_, window, cx| on_click(window, cx))
                .child(div().flex_1().ellipsis().child(truncated(a.label)))
        });

        let sheet = div()
            .id(self.id.clone())
            .occlude()
            .when_some(self.focus, |d, focus| d.track_focus(&focus))
            .key_context(KEY_CONTEXT)
            .on_action(move |_: &ClosePane, window, cx| close_key(window, cx))
            .on_mouse_down_out(move |_, window, cx| close_out(window, cx))
            .relative()
            .w_full()
            // Scale in: a touch narrower and lower at the start.
            .max_w(self.max_width * (0.96 + 0.04 * p))
            .mt(px(10.) * (1. - p))
            .max_h_full()
            .flex()
            .flex_col()
            .gap(px(14.))
            .pt(px(16.))
            .pb(px(if has_actions { 12. } else { 18. }))
            .px(px(18.))
            .rounded(RADIUS)
            .border_1()
            .border_color(glass.border)
            .bg(glass.fill())
            .shadow(glass.shadow.to_vec())
            .opacity(p)
            .text_color(t.ink)
            // The glass lip: a bright hairline along the top inner edge.
            .child(
                div()
                    .absolute()
                    .top(px(0.5))
                    .left(RADIUS * 0.6)
                    .right(RADIUS * 0.6)
                    .h(px(1.))
                    .bg(glass.highlight),
            )
            .child(header)
            .child(
                div()
                    .id("pane-body")
                    .flex_shrink()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .pl(px(26.))
                    .pr(px(4.))
                    .text_size(px(15.))
                    .line_height(px(23.))
                    .text_color(t.ink_soft)
                    .children(self.body),
            )
            .when(has_actions, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .gap(px(6.))
                        .child(
                            div()
                                .mx(px(2.))
                                .mb(px(2.))
                                .h(px(3.))
                                .rounded_full()
                                .bg(glass.divider),
                        )
                        .children(rows),
                )
            });

        div()
            .absolute()
            .inset_0()
            .flex()
            .map(|d| {
                if self.anchor_top {
                    d.items_start()
                } else {
                    d.items_center()
                }
            })
            .justify_center()
            .p(px(16.))
            .when(self.scrim, |d| {
                d.child(
                    div()
                        .id("pane-scrim")
                        .absolute()
                        .inset_0()
                        .occlude()
                        .bg(glass.scrim.opacity(p)),
                )
            })
            .child(sheet)
    }
}

/// A small on/off switch for settings rows.
pub fn switch(id: impl Into<ElementId>, on: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let t = cx.theme();
    let (track, knob) = if on {
        (t.accent, gpui::white())
    } else {
        (t.line_strong, t.surface)
    };
    div()
        .id(id)
        .flex_none()
        .w(px(36.))
        .h(px(22.))
        .p(px(2.))
        .flex()
        .when(on, |d| d.justify_end())
        .rounded_full()
        .bg(track)
        .cursor_pointer()
        .child(div().size(px(18.)).rounded_full().bg(knob).shadow_sm())
}

#[cfg(test)]
mod tests {
    use super::*;

    type State = PaneState<&'static str, u32>;

    #[test]
    fn opens_and_hands_focus_back() {
        let now = Instant::now();
        let mut s = State::new();
        assert!(!s.is_open());
        assert_eq!(s.close(), None, "closing a closed pane does nothing");
        s.open("message", Some(7), now);
        assert!(s.is_open());
        assert_eq!(s.content(), Some(&"message"));
        assert_eq!(s.close(), Some(Some(7)));
        assert!(!s.is_open());
        assert_eq!(s.close(), None);
    }

    #[test]
    fn nothing_focused_means_nothing_to_restore() {
        let mut s = State::new();
        s.open("settings", None, Instant::now());
        assert_eq!(s.close(), Some(None));
    }

    #[test]
    fn replacing_content_keeps_the_original_focus() {
        let now = Instant::now();
        let mut s = State::new();
        s.open("first", Some(1), now);
        // The second open happens with focus inside the pane (2); closing must
        // still go back to where the user was before the first one.
        s.open("second", Some(2), now + Duration::from_millis(500));
        assert_eq!(s.content(), Some(&"second"));
        assert_eq!(s.close(), Some(Some(1)));
    }

    #[test]
    fn replacing_content_does_not_replay_the_animation() {
        let now = Instant::now();
        let mut s = State::new();
        s.open("first", None, now);
        let later = now + OPEN_DURATION * 2;
        s.open("second", None, later);
        assert_eq!(s.progress(later), 1.);
        assert!(!s.animating(later));
    }

    #[test]
    fn generation_changes_with_every_open() {
        let now = Instant::now();
        let mut s = State::new();
        let g0 = s.generation();
        s.open("a", None, now);
        let g1 = s.generation();
        s.open("b", None, now);
        assert!(g0 < g1 && g1 < s.generation());
    }

    #[test]
    fn open_animation_eases_out_over_140ms() {
        let now = Instant::now();
        let mut s = State::new();
        assert_eq!(s.progress(now), 0.);
        s.open("x", None, now);
        assert_eq!(s.progress(now), 0.);
        assert!(s.animating(now));
        let mid = s.progress(now + OPEN_DURATION / 2);
        assert!(
            mid > 0.5 && mid < 1.,
            "ease-out is past halfway at the midpoint"
        );
        assert_eq!(s.progress(now + OPEN_DURATION), 1.);
        assert!(!s.animating(now + OPEN_DURATION));
    }
}
