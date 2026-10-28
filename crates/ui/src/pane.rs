//! The pane: a fixed glass overlay that shows what you need or what you
//! clicked on without taking you away from what is behind it.
//!
//! Two halves:
//!
//! - [`PaneState`]: plain open/close/focus bookkeeping with no window, so it is
//!   unit-tested. It remembers where keyboard focus was when the pane opened,
//!   hands it back on close, and times the open and close animations.
//! - [`Pane`]: the element. A red close dot pinned into the top-left corner
//!   (traffic-light sized, with a hover glyph) with a header row beside it,
//!   like a macOS title bar: the title (or a custom header such as search's
//!   field) centred on the dot, then a scrollable body that
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
    AnyElement, App, ElementId, FocusHandle, IntoElement, KeyBinding, Pixels, SharedString, Window,
    actions, div, prelude::*, px, rgb, svg,
};

use crate::{
    ActiveTheme, Ellipsis, IconName, TypeStyleExt,
    tokens::{radius, size, space, text},
    truncated,
};

actions!(pane, [ClosePane]);

/// Key context on the pane (Escape closes it).
pub const KEY_CONTEXT: &str = "Pane";
/// The fade/scale-in, matching the search grow and the sidebar peek.
pub const OPEN_DURATION: Duration = Duration::from_millis(140);
/// The fade/scale-out on close, a touch quicker than the open.
pub const CLOSE_DURATION: Duration = Duration::from_millis(130);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", ClosePane, Some(KEY_CONTEXT))]);
}

/// Open/close state for one pane slot, generic over what it shows (`C`) and
/// the focus token to hand back (`F`, a `FocusHandle` in the app).
///
/// Closing is two steps: [`PaneState::close`] ends the pane at once (it is no
/// longer open, its content no longer answers actions, focus is handed back)
/// and keeps the content only to draw the fade-out, which [`PaneState::shown`]
/// returns until [`CLOSE_DURATION`] has passed. Opening during the fade picks
/// the animation up from where the fade had got to.
#[derive(Debug)]
pub struct PaneState<C, F> {
    open: Option<Opened<C, F>>,
    closing: Option<Closing<C>>,
    /// Bumped on every open or content swap, so the body's scroll position
    /// (keyed by element id) starts at the top for new content.
    generation: u64,
}

#[derive(Debug)]
struct Opened<C, F> {
    content: C,
    return_focus: Option<F>,
    /// When the open animation started, shifted back when it resumes a fade
    /// so the curve continues from the visible progress.
    opened_at: Instant,
}

#[derive(Debug)]
struct Closing<C> {
    content: C,
    closed_at: Instant,
    /// Visible progress when the close began (below 1 when closed mid-open).
    from: f32,
}

impl<C, F> Default for PaneState<C, F> {
    fn default() -> Self {
        Self {
            open: None,
            closing: None,
            generation: 0,
        }
    }
}

fn ease_out(t: f32) -> f32 {
    1. - (1. - t.clamp(0., 1.)).powi(3)
}

/// The `t` at which [`ease_out`] reaches `p`.
fn ease_out_inverse(p: f32) -> f32 {
    1. - (1. - p.clamp(0., 1.)).cbrt()
}

fn fraction(since: Instant, now: Instant, of: Duration) -> f32 {
    (now.saturating_duration_since(since).as_secs_f32() / of.as_secs_f32()).min(1.)
}

impl<C, F> PaneState<C, F> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open and interactive. False while the close fade plays.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Whether the close fade is still playing at `now`.
    pub fn is_closing(&self, now: Instant) -> bool {
        self.open.is_none()
            && self
                .closing
                .as_ref()
                .is_some_and(|c| now.saturating_duration_since(c.closed_at) < CLOSE_DURATION)
    }

    /// The open pane's content. `None` while closing, so actions can't run.
    pub fn content(&self) -> Option<&C> {
        self.open.as_ref().map(|o| &o.content)
    }

    pub fn content_mut(&mut self) -> Option<&mut C> {
        self.open.as_mut().map(|o| &mut o.content)
    }

    /// What to draw at `now`: the open content, or the closing content until
    /// its fade has finished.
    pub fn shown(&self, now: Instant) -> Option<&C> {
        match &self.open {
            Some(open) => Some(&open.content),
            None if self.is_closing(now) => self.closing.as_ref().map(|c| &c.content),
            None => None,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Shows `content`. `focused` is whatever had the keyboard just before.
    ///
    /// Opening over an open pane swaps the content in place: no second
    /// animation, and focus still returns to where it was before the first
    /// pane, not to the pane being replaced. Opening during the close fade
    /// starts a fresh pane whose open animation resumes from the fade's
    /// current progress, so nothing jumps.
    pub fn open(&mut self, content: C, focused: Option<F>, now: Instant) {
        self.generation += 1;
        match &mut self.open {
            Some(open) => open.content = content,
            None => {
                let resume = self.progress(now);
                self.closing = None;
                let back = OPEN_DURATION.mul_f32(ease_out_inverse(resume));
                self.open = Some(Opened {
                    content,
                    return_focus: focused,
                    opened_at: now.checked_sub(back).unwrap_or(now),
                })
            }
        }
    }

    /// Closes the pane and starts the fade-out. `None` when it was already
    /// closed (or closing); otherwise the focus to restore (itself `None`
    /// when nothing had focus before).
    pub fn close(&mut self, now: Instant) -> Option<Option<F>> {
        let from = self.progress(now);
        let open = self.open.take()?;
        self.closing = Some(Closing {
            content: open.content,
            closed_at: now,
            from,
        });
        Some(open.return_focus)
    }

    /// 0..1 visible amount (opacity, scale): eased out (cubic) up to 1 over
    /// [`OPEN_DURATION`] while opening, back down to 0 over
    /// [`CLOSE_DURATION`] while closing. 0 when nothing is shown.
    pub fn progress(&self, now: Instant) -> f32 {
        if let Some(open) = &self.open {
            return ease_out(fraction(open.opened_at, now, OPEN_DURATION));
        }
        match &self.closing {
            Some(c) => c.from * (1. - ease_out(fraction(c.closed_at, now, CLOSE_DURATION))),
            None => 0.,
        }
    }

    /// Whether an open or close animation still needs frames.
    pub fn animating(&self, now: Instant) -> bool {
        (self.is_open() && self.progress(now) < 1.) || self.is_closing(now)
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
    closing: bool,
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
            closing: false,
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

    /// The pane is fading out: it draws but takes no clicks, keys or focus
    /// changes, so whatever is behind it works straight away.
    pub fn closing(mut self, closing: bool) -> Self {
        self.closing = closing;
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

    /// Something other than a title, on the close dot's row beside it
    /// (search puts its field here). Replaces the title.
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
const RADIUS: Pixels = radius::XL;
/// The close dot: macOS traffic-light diameter.
pub const CLOSE_DOT: Pixels = px(12.);
/// How far the dot sits from the pane's top and left edges.
pub const CLOSE_INSET: Pixels = space::X3;
/// Extra hit area around the dot on every side (hover shows the glyph).
pub const CLOSE_SLOP: Pixels = space::X2;
/// The sheet's content padding left and right.
const PAD_X: Pixels = space::X4;
/// The sheet's border. Padding and absolute offsets both start inside it.
const BORDER: Pixels = size::HAIRLINE;
/// Top padding with neither title nor header: the body clears the dot.
const PAD_TOP_BARE: Pixels = size::BUTTON;
/// Height of the header row (title or custom header), centred on the dot.
const HEADER_H: Pixels = size::BUTTON;
/// The title, on the dot's row: smaller than a page heading so the row stays
/// compact, still clearly the pane's title.
pub const TITLE_SIZE: Pixels = text::TITLE.size;
/// Gap between the dot's visual right edge and the title.
pub const TITLE_GAP: Pixels = space::X2;
/// Gap between the dot's visual right edge and a custom header (search's
/// field, which has its own icon to lead with).
const HEADER_GAP: Pixels = space::X3;

/// Top padding that centres a [`HEADER_H`] row on the dot.
fn header_pad_top() -> Pixels {
    CLOSE_INSET + CLOSE_DOT / 2. - HEADER_H / 2. - BORDER
}

/// Left margin, inside the content padding, that starts the header row `gap`
/// past the dot's visual right edge.
fn header_margin_left(gap: Pixels) -> Pixels {
    CLOSE_INSET + CLOSE_DOT + gap - PAD_X - BORDER
}

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

        let closing = self.closing;
        let group: SharedString = "pane-close".into();
        // The hit area: the dot plus CLOSE_SLOP all round, pinned so the dot
        // itself lands CLOSE_INSET from the top and left edges.
        let dot = div()
            .id("pane-close")
            .group(group.clone())
            .absolute()
            // Absolute offsets start inside the 1px border.
            .top(CLOSE_INSET - CLOSE_SLOP - BORDER)
            .left(CLOSE_INSET - CLOSE_SLOP - BORDER)
            .size(CLOSE_DOT + CLOSE_SLOP * 2.)
            .flex()
            .items_center()
            .justify_center()
            .when(!closing, |d| {
                d.cursor_pointer().on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    close_dot(window, cx)
                })
            })
            .child(
                div()
                    .size(CLOSE_DOT)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(CLOSE_RIM))
                    .bg(rgb(CLOSE_RED))
                    .child(
                        div().invisible().group_hover(group, |s| s.visible()).child(
                            svg()
                                .path(IconName::X.path())
                                // Optical: the glyph inside a 12px traffic light.
                                .size(px(8.))
                                .text_color(rgb(CLOSE_GLYPH)),
                        ),
                    ),
            );

        // The dot's row, like a macOS title bar: the title (or a custom
        // header) centred on the dot and starting just past it. The body
        // follows straight under the row.
        let row = |gap: Pixels| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .h(HEADER_H)
                .min_w_0()
                .ml(header_margin_left(gap))
        };
        let header = match (self.header, self.title) {
            (Some(header), _) => {
                Some(row(HEADER_GAP).child(div().flex_1().min_w_0().child(header)))
            }
            (None, Some(title)) => Some(
                row(TITLE_GAP).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .ellipsis()
                        .type_style(text::TITLE)
                        .text_color(t.ink)
                        .child(truncated(title)),
                ),
            ),
            (None, None) => None,
        };
        let pad_top = if header.is_some() {
            header_pad_top()
        } else {
            PAD_TOP_BARE
        };

        let has_actions = !self.actions.is_empty();
        let rows = self.actions.into_iter().map(|a| {
            let on_click = a.on_click.clone();
            div()
                .id(a.id)
                .flex()
                .flex_none()
                .items_center()
                .w_full()
                .h(size::ROW)
                .px(space::X3)
                .rounded(radius::MD)
                .bg(glass.well)
                .hover(|s| s.bg(glass.well_hover))
                .type_style(text::SMALL)
                .text_color(t.ink)
                .when(!closing, |d| {
                    d.cursor_pointer()
                        .on_click(move |_, window, cx| on_click(window, cx))
                })
                .child(div().flex_1().ellipsis().child(truncated(a.label)))
        });

        let sheet = div()
            .id(self.id.clone())
            .when_some(self.focus, |d, focus| d.track_focus(&focus))
            // While fading out it only draws: clicks go through to what is
            // behind it and keys go wherever focus was handed back to.
            .when(!closing, |d| {
                d.occlude()
                    .key_context(KEY_CONTEXT)
                    .on_action(move |_: &ClosePane, window, cx| close_key(window, cx))
                    .on_mouse_down_out(move |_, window, cx| close_out(window, cx))
            })
            .relative()
            .w_full()
            // Scale in (and back out on close): a touch narrower and lower.
            .max_w(self.max_width * (0.96 + 0.04 * p))
            .mt(space::X2 * (1. - p))
            .max_h_full()
            .flex()
            .flex_col()
            .gap(space::X3)
            .pt(pad_top)
            .pb(if has_actions { space::X3 } else { space::X4 })
            .px(PAD_X)
            .rounded(RADIUS)
            .border_1()
            .border_color(glass.border)
            .bg(glass.fill)
            .shadow(glass.shadow.to_vec())
            .opacity(p)
            .text_color(t.ink)
            // The glass lip: a bright hairline along the top inner edge.
            .child(
                div()
                    .absolute()
                    // Optical: half a pixel in, so the lip sits on the border.
                    .top(px(0.5))
                    .left(RADIUS * 0.6)
                    .right(RADIUS * 0.6)
                    .h(size::HAIRLINE)
                    .bg(glass.highlight),
            )
            .children(header)
            .child(
                div()
                    .id("pane-body")
                    .flex_shrink()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(space::X2)
                    .pr(space::X1)
                    .type_style(text::BODY)
                    .text_color(t.ink_soft)
                    .children(self.body),
            )
            .when(has_actions, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .gap(space::X1_5)
                        // A hairline, not a bar.
                        .child(div().mb(space::X0_5).h(size::HAIRLINE).bg(glass.divider))
                        .children(rows),
                )
            })
            // Last, so it paints and hit-tests above the header and body.
            .child(dot);

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
            .p(space::X4)
            .when(self.scrim, |d| {
                d.child(
                    div()
                        .id("pane-scrim")
                        .absolute()
                        .inset_0()
                        .when(!closing, |d| d.occlude())
                        .bg(glass.scrim.opacity(p)),
                )
            })
            .child(sheet)
    }
}

/// Switch track width: a chip-high track, wide enough for the knob to travel.
const SWITCH_W: Pixels = size::ROW;

/// A small on/off switch for settings rows.
pub fn switch(id: impl Into<ElementId>, on: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let t = cx.theme();
    let (track, knob) = if on {
        (t.accent, t.on_accent)
    } else {
        (t.line_strong, t.surface)
    };
    div()
        .id(id)
        .flex_none()
        .w(SWITCH_W)
        .h(size::CHIP)
        .p(space::X0_5)
        .flex()
        .when(on, |d| d.justify_end())
        .rounded_full()
        .bg(track)
        .cursor_pointer()
        .child(
            div()
                .size(size::CHIP - space::X0_5 * 2.)
                .rounded_full()
                .bg(knob),
        )
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
        assert_eq!(s.close(now), None, "closing a closed pane does nothing");
        s.open("message", Some(7), now);
        assert!(s.is_open());
        assert_eq!(s.content(), Some(&"message"));
        assert_eq!(s.close(now), Some(Some(7)));
        assert!(!s.is_open());
        assert_eq!(s.close(now), None);
    }

    #[test]
    fn nothing_focused_means_nothing_to_restore() {
        let now = Instant::now();
        let mut s = State::new();
        s.open("settings", None, now);
        assert_eq!(s.close(now), Some(None));
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
        assert_eq!(s.close(now), Some(Some(1)));
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

    #[test]
    fn close_fades_out_over_130ms_and_stops_taking_input() {
        let now = Instant::now();
        let mut s = State::new();
        s.open("x", Some(3), now);
        let settled = now + OPEN_DURATION;
        assert_eq!(s.progress(settled), 1.);
        // Focus comes back the moment the close starts, not after the fade.
        assert_eq!(s.close(settled), Some(Some(3)));
        assert!(!s.is_open(), "not interactive while fading");
        assert_eq!(s.content(), None, "actions can't reach closing content");
        assert_eq!(s.shown(settled), Some(&"x"), "still drawn");
        assert!(s.is_closing(settled));
        assert!(s.animating(settled));
        assert_eq!(s.progress(settled), 1.);
        let mid = s.progress(settled + CLOSE_DURATION / 2);
        assert!(mid > 0. && mid < 0.5, "eases out: mostly gone halfway");
        let done = settled + CLOSE_DURATION;
        assert_eq!(s.progress(done), 0.);
        assert!(!s.is_closing(done));
        assert!(!s.animating(done));
        assert_eq!(s.shown(done), None, "gone once the fade ends");
        assert_eq!(s.close(done), None, "closing twice does nothing");
    }

    #[test]
    fn close_mid_open_fades_from_where_it_got_to() {
        let now = Instant::now();
        let mut s = State::new();
        s.open("x", None, now);
        let at = now + OPEN_DURATION / 4;
        let visible = s.progress(at);
        assert!(visible > 0. && visible < 1.);
        s.close(at);
        assert_eq!(s.progress(at), visible, "no jump to fully open");
        assert!(s.progress(at + CLOSE_DURATION / 3) < visible);
        assert_eq!(s.progress(at + CLOSE_DURATION), 0.);
    }

    #[test]
    fn reopening_mid_close_resumes_without_a_jump() {
        let now = Instant::now();
        let mut s = State::new();
        s.open("first", Some(1), now);
        let closed = now + OPEN_DURATION;
        s.close(closed);
        let at = closed + CLOSE_DURATION / 3;
        let visible = s.progress(at);
        assert!(visible > 0. && visible < 1.);
        let generation = s.generation();
        // Focus is back on 1 by now, so the new pane remembers 1 (or
        // whatever has it at this point).
        s.open("second", Some(4), at);
        assert!(s.is_open());
        assert!(!s.is_closing(at));
        assert_eq!(s.content(), Some(&"second"));
        assert_eq!(s.shown(at), Some(&"second"));
        assert!(s.generation() > generation, "fresh scroll for new content");
        assert!(
            (s.progress(at) - visible).abs() < 1e-3,
            "open picks up from the fade: {} vs {visible}",
            s.progress(at)
        );
        assert!(s.animating(at));
        assert!(s.progress(at + OPEN_DURATION / 4) > visible);
        assert_eq!(s.progress(at + OPEN_DURATION), 1.);
        assert_eq!(s.close(at + OPEN_DURATION), Some(Some(4)));
    }

    #[test]
    fn reopening_after_the_fade_plays_the_full_open() {
        let now = Instant::now();
        let mut s = State::new();
        s.open("a", None, now);
        let closed = now + OPEN_DURATION;
        s.close(closed);
        let later = closed + CLOSE_DURATION * 2;
        s.open("b", None, later);
        assert_eq!(s.progress(later), 0.);
        assert_eq!(s.progress(later + OPEN_DURATION), 1.);
    }

    #[test]
    fn close_dot_is_traffic_light_sized_in_the_corner() {
        assert_eq!(CLOSE_DOT, px(12.));
        assert_eq!(CLOSE_INSET, px(12.));
        assert_eq!(CLOSE_SLOP, px(8.));
        assert!(
            PAD_TOP_BARE > CLOSE_INSET + CLOSE_DOT,
            "a pane with no header row starts its body below the dot"
        );
        assert!(CLOSE_DURATION >= Duration::from_millis(120) && CLOSE_DURATION <= OPEN_DURATION);
    }

    /// The header row sits on the dot's row like a macOS title bar, measured
    /// from the sheet's outer edge (border included).
    #[test]
    fn title_shares_the_dot_row() {
        let dot_top = CLOSE_INSET;
        let dot_centre = dot_top + CLOSE_DOT / 2.;
        let row_top = BORDER + header_pad_top();
        assert_eq!(
            row_top + HEADER_H / 2.,
            dot_centre,
            "row centred on the dot"
        );
        assert!(row_top >= px(0.));
        assert!(text::TITLE.line <= HEADER_H);

        let dot_right = CLOSE_INSET + CLOSE_DOT;
        let title_left = BORDER + PAD_X + header_margin_left(TITLE_GAP);
        assert_eq!(title_left - dot_right, TITLE_GAP);
        assert!(TITLE_GAP >= px(8.) && TITLE_GAP <= px(10.));
        assert!(
            title_left >= dot_right + CLOSE_SLOP,
            "the title clears the dot's hit area"
        );
        let header_left = BORDER + PAD_X + header_margin_left(HEADER_GAP);
        assert_eq!(header_left - dot_right, HEADER_GAP);

        assert!(
            TITLE_SIZE >= px(15.) && TITLE_SIZE < px(21.),
            "compact but a title"
        );
    }
}
