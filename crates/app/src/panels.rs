//! The two sidebars: whether each is docked beside the chat or collapsed, the
//! edge "peek" that slides a collapsed one back in as an overlay, and the short
//! slides between those states.
//!
//! Everything here is plain state driven by the window width, the pointer and
//! the clock, so the rules can be unit-tested. `Workspace` feeds it events and
//! draws what `frame()` says. The rules are written up under "Sidebars" in
//! ARCHITECTURE.md; keep the two in step.

use std::time::{Duration, Instant};

use gpui::{Context, Pixels, Point, Size, Task, actions, px};

use crate::workspace::TITLEBAR_HEIGHT;

actions!(chaos_control, [ToggleLeftSidebar, ToggleRightSidebar]);

pub const LEFT_WIDTH: Pixels = px(258.);
pub const RIGHT_WIDTH: Pixels = px(320.);
/// Gap between the window edge and a floating panel. The sidebars use it on the
/// sides and the bottom; the composer uses it at the bottom.
pub const SIDE_INSET: Pixels = ui::tokens::space::X3;
/// Breathing room between a sidebar and the centre column.
pub const GUTTER: Pixels = px(20.);

/// A sidebar in `Auto` docks only while the centre column keeps at least this
/// width next to it. With both docked that puts the breakpoint at 1202px (the
/// old fixed 1200px right-sidebar cut-off), and the left one alone at 870px.
pub const FIT_COLUMN: f32 = 560.;
/// A sidebar the user pinned open docks as long as the column keeps this much.
pub const FLOOR_COLUMN: f32 = 360.;
/// An `Auto` sidebar that collapsed comes back only once the window is this
/// much wider than its breakpoint, so dragging a window edge back and forth
/// across the line doesn't make it flap.
pub const HYSTERESIS: f32 = 24.;

/// Width of the invisible strip at each window edge that peeks a collapsed
/// sidebar. It starts under the titlebar, so on macOS it never overlaps the
/// traffic lights or the titlebar toggles.
pub const EDGE_ZONE: Pixels = px(10.);
/// Once peeking, the pointer can wander this far past the panel's outer edge
/// before it counts as having left.
const PEEK_SLACK: Pixels = px(12.);
/// How long a peek stays after the pointer leaves it.
pub const PEEK_GRACE: Duration = Duration::from_millis(350);

/// Docking and undocking reflow the column over this long.
const DOCK_SLIDE: Duration = Duration::from_millis(180);
/// A peek slides in and out a little faster: it's a glance, not a layout change.
const PEEK_SLIDE: Duration = Duration::from_millis(140);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    const BOTH: [Side; 2] = [Side::Left, Side::Right];

    fn ix(self) -> usize {
        self as usize
    }

    /// Horizontal space a docked sidebar takes from its edge, gutter included.
    fn docked_reserve(self) -> f32 {
        let width = match self {
            Side::Left => LEFT_WIDTH,
            Side::Right => RIGHT_WIDTH,
        };
        f32::from(SIDE_INSET + width + GUTTER)
    }

    pub fn width(self) -> Pixels {
        match self {
            Side::Left => LEFT_WIDTH,
            Side::Right => RIGHT_WIDTH,
        }
    }
}

/// What the user asked for. `Auto` is the default for both sidebars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Docked while it fits beside a `FIT_COLUMN` chat, collapsed otherwise.
    Auto,
    /// Docked at any width (down to `FLOOR_COLUMN`) until the user hides it.
    Pinned,
    /// Collapsed at every width; only the edge peek shows it.
    Collapsed,
}

/// Width left for the centre column with the given sidebars docked.
fn column_width(viewport: f32, docked: [bool; 2]) -> f32 {
    let reserve = |side: Side| {
        if docked[side.ix()] {
            side.docked_reserve()
        } else {
            f32::from(GUTTER)
        }
    };
    viewport - reserve(Side::Left) - reserve(Side::Right)
}

/// Which sidebars dock at this width.
///
/// Pinned sidebars are placed first, the most recently pinned first, each as
/// long as the column keeps `FLOOR_COLUMN`. `Auto` ones follow, left before
/// right, each as long as the column keeps `FIT_COLUMN` (plus `HYSTERESIS` if
/// it was collapsed). `Collapsed` never docks.
pub fn plan(
    viewport: f32,
    modes: [Mode; 2],
    pin_seq: [u64; 2],
    was_docked: [bool; 2],
) -> [bool; 2] {
    let mut docked = [false; 2];
    let mut pinned: Vec<Side> = Side::BOTH
        .into_iter()
        .filter(|s| modes[s.ix()] == Mode::Pinned)
        .collect();
    pinned.sort_by_key(|s| std::cmp::Reverse(pin_seq[s.ix()]));
    for side in pinned {
        docked[side.ix()] = true;
        if column_width(viewport, docked) < FLOOR_COLUMN {
            docked[side.ix()] = false;
        }
    }
    for side in Side::BOTH {
        if modes[side.ix()] != Mode::Auto {
            continue;
        }
        let need = FIT_COLUMN
            + if was_docked[side.ix()] {
                0.
            } else {
                HYSTERESIS
            };
        docked[side.ix()] = true;
        if column_width(viewport, docked) < need {
            docked[side.ix()] = false;
        }
    }
    docked
}

/// A 0..1 value easing toward its target, reversible mid-way.
#[derive(Clone, Copy, Debug)]
struct Slide {
    open: bool,
    /// Progress when the last change happened, so reversing mid-slide is smooth.
    from: f32,
    started: Option<Instant>,
    duration: Duration,
}

impl Slide {
    fn new(open: bool, duration: Duration) -> Self {
        Self {
            open,
            from: if open { 1. } else { 0. },
            started: None,
            duration,
        }
    }

    fn set(&mut self, open: bool, now: Instant) {
        if open != self.open {
            self.from = self.progress_at(now).0;
            self.open = open;
            self.started = Some(now);
        }
    }

    /// Jump straight to the target with no animation.
    fn snap(&mut self, open: bool) {
        *self = Self::new(open, self.duration);
    }

    /// How far along it is (0 closed, 1 open, eased) and whether it's moving.
    fn progress_at(&self, now: Instant) -> (f32, bool) {
        let target = if self.open { 1. } else { 0. };
        let Some(started) = self.started else {
            return (target, false);
        };
        let t = (now.saturating_duration_since(started).as_secs_f32()
            / self.duration.as_secs_f32())
        .min(1.);
        let eased = 1. - (1. - t).powi(3);
        (self.from + (target - self.from) * eased, t < 1.)
    }
}

/// The edge peek of one collapsed sidebar.
///
/// Opens when the pointer enters the edge zone (not while a button is held, so
/// a text drag that runs off the edge doesn't pop it). While open, the panel
/// and the strip between it and the edge count as inside. Leaving starts a
/// `PEEK_GRACE` countdown that coming back cancels. It never closes while held
/// (keyboard focus or an open menu inside it); when the hold ends with the
/// pointer outside, the grace starts then.
#[derive(Clone, Debug)]
pub struct Peek {
    open: bool,
    slide: Slide,
    inside: bool,
    left_at: Option<Instant>,
    held: bool,
}

impl Peek {
    fn new() -> Self {
        Self {
            open: false,
            slide: Slide::new(false, PEEK_SLIDE),
            inside: false,
            left_at: None,
            held: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Returns whether anything visible changed.
    fn pointer(&mut self, in_zone: bool, in_panel: bool, pressed: bool, now: Instant) -> bool {
        if !self.open {
            if in_zone && !pressed {
                self.open = true;
                self.inside = true;
                self.left_at = None;
                self.slide.set(true, now);
                return true;
            }
            return false;
        }
        self.inside = in_zone || in_panel;
        if self.inside {
            self.left_at = None;
        } else if self.left_at.is_none() {
            self.left_at = Some(now);
        }
        false
    }

    fn set_held(&mut self, held: bool, now: Instant) {
        if self.held && !held && self.open && !self.inside {
            self.left_at = Some(now);
        }
        self.held = held;
    }

    /// When the pending close is due, if one is.
    fn deadline(&self) -> Option<Instant> {
        if !self.open || self.held {
            return None;
        }
        self.left_at.map(|at| at + PEEK_GRACE)
    }

    /// Closes the peek if its grace ran out. Returns whether it closed.
    fn poll(&mut self, now: Instant) -> bool {
        if self.deadline().is_some_and(|due| now >= due) {
            self.close(now);
            return true;
        }
        false
    }

    fn close(&mut self, now: Instant) {
        self.open = false;
        self.inside = false;
        self.left_at = None;
        self.slide.set(false, now);
    }
}

struct SidebarState {
    mode: Mode,
    /// Bumped each time this sidebar is pinned; the newest pin wins for space.
    pin_seq: u64,
    docked: bool,
    dock: Slide,
    /// Docked straight from a peek: the panel is already in place, so only
    /// the column moves.
    docked_from_peek: bool,
    peek: Peek,
}

impl SidebarState {
    fn new() -> Self {
        Self {
            mode: Mode::Auto,
            pin_seq: 0,
            docked: true,
            dock: Slide::new(true, DOCK_SLIDE),
            docked_from_peek: false,
            peek: Peek::new(),
        }
    }
}

/// How to draw one sidebar this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SideFrame {
    /// 0..1 share of its docked width the column gives up for it.
    pub reserve: f32,
    /// 0..1 how far it has slid in from its edge.
    pub shown: f32,
    /// Drawn over the content (peeking) rather than beside it.
    pub overlay: bool,
}

/// Owns both sidebars' state. Pure logic over `Instant`s plus a timer for the
/// peek grace; nothing here touches a window.
pub struct Panels {
    sides: [SidebarState; 2],
    next_pin: u64,
    viewport: Size<Pixels>,
    /// The close the grace timer is armed for, so pointer moves don't re-arm it.
    armed: Option<Instant>,
    _grace_timer: Option<Task<()>>,
}

impl Panels {
    pub fn new() -> Self {
        Self {
            sides: [SidebarState::new(), SidebarState::new()],
            next_pin: 1,
            viewport: Size::default(),
            armed: None,
            _grace_timer: None,
        }
    }

    fn side(&self, side: Side) -> &SidebarState {
        &self.sides[side.ix()]
    }

    fn side_mut(&mut self, side: Side) -> &mut SidebarState {
        &mut self.sides[side.ix()]
    }

    pub fn mode(&self, side: Side) -> Mode {
        self.side(side).mode
    }

    /// Docked beside the chat right now (after resize rules).
    pub fn docked(&self, side: Side) -> bool {
        self.side(side).docked
    }

    #[cfg(test)]
    pub fn peeking(&self, side: Side) -> bool {
        self.side(side).peek.is_open()
    }

    fn modes(&self) -> [Mode; 2] {
        [self.sides[0].mode, self.sides[1].mode]
    }

    fn pin_seqs(&self) -> [u64; 2] {
        [self.sides[0].pin_seq, self.sides[1].pin_seq]
    }

    fn docked_now(&self) -> [bool; 2] {
        [self.sides[0].docked, self.sides[1].docked]
    }

    /// Re-applies the rules for a window size. Returns whether either sidebar
    /// docked or collapsed.
    pub fn layout(&mut self, viewport: Size<Pixels>, now: Instant) -> bool {
        self.viewport = viewport;
        let target = plan(
            f32::from(viewport.width),
            self.modes(),
            self.pin_seqs(),
            self.docked_now(),
        );
        let mut changed = false;
        for side in Side::BOTH {
            changed |= self.set_docked(side, target[side.ix()], now);
        }
        changed
    }

    fn set_docked(&mut self, side: Side, docked: bool, now: Instant) -> bool {
        let s = self.side_mut(side);
        if s.docked == docked {
            return false;
        }
        s.docked = docked;
        s.docked_from_peek = docked && s.peek.is_open();
        if s.docked_from_peek {
            // The overlay becomes the docked panel where it stands.
            s.peek.open = false;
            s.peek.inside = false;
            s.peek.left_at = None;
            s.peek.slide.snap(false);
        }
        s.dock.set(docked, now);
        true
    }

    /// The titlebar toggle and cmd/ctrl-alt-[ / ]. Hiding a docked sidebar
    /// collapses it for good (`Collapsed`). Showing one docks it now: as `Auto`
    /// when it fits, so it keeps adapting, or `Pinned` when the user asked for
    /// it at a width where it would not dock on its own.
    pub fn toggle(&mut self, side: Side, now: Instant) {
        if self.side(side).docked {
            self.side_mut(side).mode = Mode::Collapsed;
        } else {
            let mut modes = self.modes();
            modes[side.ix()] = Mode::Auto;
            let mut was = self.docked_now();
            // An explicit request needs no hysteresis margin.
            was[side.ix()] = true;
            let fits = plan(f32::from(self.viewport.width), modes, self.pin_seqs(), was)[side.ix()];
            if fits {
                self.side_mut(side).mode = Mode::Auto;
            } else {
                let seq = self.next_pin;
                self.next_pin += 1;
                let s = self.side_mut(side);
                s.mode = Mode::Pinned;
                s.pin_seq = seq;
            }
        }
        let viewport = self.viewport;
        self.layout(viewport, now);
    }

    /// Pointer moved (or left the window, with `None`). Returns whether a peek
    /// opened, so the caller can redraw; closes happen through `poll`.
    pub fn pointer(
        &mut self,
        position: Option<Point<Pixels>>,
        pressed: bool,
        now: Instant,
    ) -> bool {
        let width = self.viewport.width;
        let mut changed = false;
        for side in Side::BOTH {
            if self.side(side).docked {
                continue;
            }
            let (in_zone, in_panel) = match position {
                None => (false, false),
                Some(p) if p.y < TITLEBAR_HEIGHT => (false, false),
                Some(p) => {
                    let reach = SIDE_INSET + side.width() + PEEK_SLACK;
                    match side {
                        Side::Left => (p.x < EDGE_ZONE, p.x < reach),
                        Side::Right => (p.x >= width - EDGE_ZONE, p.x >= width - reach),
                    }
                }
            };
            changed |= self
                .side_mut(side)
                .peek
                .pointer(in_zone, in_panel, pressed, now);
        }
        changed
    }

    /// Something inside the sidebar holds the peek open (keyboard focus, an
    /// open menu or popover).
    pub fn set_held(&mut self, side: Side, held: bool, now: Instant) {
        self.side_mut(side).peek.set_held(held, now);
    }

    /// Closes any peek whose grace has run out. Returns whether one closed.
    pub fn poll(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for s in &mut self.sides {
            changed |= s.peek.poll(now);
        }
        changed
    }

    /// The soonest pending peek close.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.sides.iter().filter_map(|s| s.peek.deadline()).min()
    }

    /// Arms (or re-arms) the grace timer for the soonest pending close.
    pub fn schedule_poll(&mut self, cx: &mut Context<Self>) {
        let due = self.next_deadline();
        if due == self.armed {
            return;
        }
        self.armed = due;
        let Some(due) = due else {
            self._grace_timer = None;
            return;
        };
        let wait = due.saturating_duration_since(Instant::now());
        self._grace_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |this, cx| {
                this.armed = None;
                if this.poll(Instant::now()) {
                    cx.notify();
                }
                this.schedule_poll(cx);
            })
            .ok();
        }));
    }

    /// What to draw for each sidebar, and whether anything is still moving.
    pub fn frame(&self, now: Instant) -> ([SideFrame; 2], bool) {
        let mut moving = false;
        let frames = [0, 1].map(|ix| {
            let s = &self.sides[ix];
            let (dock, dock_moving) = s.dock.progress_at(now);
            let (peek, peek_moving) = s.peek.slide.progress_at(now);
            moving |= dock_moving || peek_moving;
            let shown = if s.docked && s.docked_from_peek {
                1.
            } else {
                dock.max(peek)
            };
            SideFrame {
                reserve: dock,
                shown,
                overlay: !s.docked && peek > 0.,
            }
        });
        (frames, moving)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    use Mode::*;

    const NONE: [u64; 2] = [0, 0];

    fn at(w: f32, modes: [Mode; 2]) -> [bool; 2] {
        plan(w, modes, NONE, [true, true])
    }

    #[test]
    fn auto_breakpoints() {
        // Both fit from 1202px; below, the right one goes first.
        assert_eq!(at(1440., [Auto, Auto]), [true, true]);
        assert_eq!(at(1202., [Auto, Auto]), [true, true]);
        assert_eq!(at(1201., [Auto, Auto]), [true, false]);
        // The left one alone needs 870px.
        assert_eq!(at(870., [Auto, Auto]), [true, false]);
        assert_eq!(at(869., [Auto, Auto]), [false, false]);
        // With the left one hidden by hand, the right one has room sooner.
        assert_eq!(at(1000., [Collapsed, Auto]), [false, true]);
        assert_eq!(at(931., [Collapsed, Auto]), [false, false]);
    }

    #[test]
    fn auto_needs_a_margin_to_come_back() {
        let was_collapsed = [true, false];
        assert_eq!(
            plan(1210., [Auto, Auto], NONE, was_collapsed),
            [true, false]
        );
        assert_eq!(plan(1226., [Auto, Auto], NONE, was_collapsed), [true, true]);
        // Already docked: stays docked right down to the breakpoint.
        assert_eq!(plan(1210., [Auto, Auto], NONE, [true, true]), [true, true]);
    }

    #[test]
    fn collapsed_never_docks() {
        assert_eq!(at(3000., [Collapsed, Collapsed]), [false, false]);
    }

    #[test]
    fn pinned_docks_below_its_breakpoint_and_takes_priority() {
        // The window minimum is 760px: a pinned right sidebar still docks.
        assert_eq!(at(760., [Auto, Pinned]), [false, true]);
        // At 1000px the pinned right one pushes the auto left one out.
        assert_eq!(at(1000., [Auto, Pinned]), [false, true]);
        // Both pinned and no room for both: the newest pin wins.
        assert_eq!(
            plan(900., [Pinned, Pinned], [1, 2], [true, true]),
            [false, true]
        );
        assert_eq!(
            plan(900., [Pinned, Pinned], [3, 2], [true, true]),
            [true, false]
        );
        // With room for both at the floor, both dock.
        assert_eq!(
            plan(1100., [Pinned, Pinned], [1, 2], [true, true]),
            [true, true]
        );
    }

    fn panels(width: f32) -> (Panels, Instant) {
        let mut p = Panels::new();
        let start = Instant::now();
        p.layout(size(px(width), px(900.)), start);
        // Let the initial dock slides settle.
        (p, start + Duration::from_secs(1))
    }

    #[test]
    fn toggles_hide_for_good_and_show_as_auto_or_pinned() {
        let (mut p, now) = panels(1440.);
        p.toggle(Side::Right, now);
        assert_eq!(p.mode(Side::Right), Collapsed);
        assert!(!p.docked(Side::Right));
        // Hidden by hand: widening doesn't bring it back.
        p.layout(size(px(2400.), px(900.)), now);
        assert!(!p.docked(Side::Right));
        // Shown where it fits: Auto, so narrowing collapses it again.
        p.toggle(Side::Right, now);
        assert_eq!(p.mode(Side::Right), Auto);
        p.layout(size(px(1100.), px(900.)), now);
        assert!(!p.docked(Side::Right));
        // Shown where it doesn't fit: Pinned, and it stays through resizes.
        p.toggle(Side::Right, now);
        assert_eq!(p.mode(Side::Right), Pinned);
        assert!(p.docked(Side::Right));
        assert!(!p.docked(Side::Left), "the pinned right one takes the room");
        p.layout(size(px(2400.), px(900.)), now);
        p.layout(size(px(1000.), px(900.)), now);
        assert!(p.docked(Side::Right));
        // Hiding a pinned one collapses it.
        p.toggle(Side::Right, now);
        assert_eq!(p.mode(Side::Right), Collapsed);
        assert!(p.docked(Side::Left), "the left one gets its room back");
    }

    #[test]
    fn auto_collapse_and_redock_follow_the_window() {
        let (mut p, now) = panels(1440.);
        assert!(p.layout(size(px(1000.), px(900.)), now));
        assert_eq!((p.docked(Side::Left), p.docked(Side::Right)), (true, false));
        assert_eq!(p.mode(Side::Right), Auto, "resizing never changes the mode");
        p.layout(size(px(1440.), px(900.)), now);
        assert!(p.docked(Side::Right));
    }

    fn near_left(y: f32) -> Option<Point<Pixels>> {
        Some(point(px(4.), px(y)))
    }

    #[test]
    fn edge_zone_peeks_and_grace_closes() {
        let (mut p, now) = panels(800.);
        assert!(!p.docked(Side::Left));
        // Inside the titlebar: nothing (traffic lights, toggles live there).
        assert!(!p.pointer(near_left(20.), false, now));
        // Dragging with a button held: nothing.
        assert!(!p.pointer(near_left(400.), true, now));
        // The edge zone opens it.
        assert!(p.pointer(near_left(400.), false, now));
        assert!(p.peeking(Side::Left));
        // Moving onto the panel keeps it, with no pending close.
        p.pointer(Some(point(px(200.), px(400.))), false, now);
        assert_eq!(p.next_deadline(), None);
        // Leaving starts the grace; coming back within it cancels.
        let t1 = now + Duration::from_millis(50);
        p.pointer(Some(point(px(600.), px(400.))), false, t1);
        assert_eq!(p.next_deadline(), Some(t1 + PEEK_GRACE));
        assert!(!p.poll(t1 + PEEK_GRACE / 2));
        p.pointer(Some(point(px(100.), px(400.))), false, t1 + PEEK_GRACE / 2);
        assert!(!p.poll(t1 + PEEK_GRACE * 2));
        assert!(p.peeking(Side::Left));
        // Leave for good.
        let t2 = t1 + PEEK_GRACE * 3;
        p.pointer(Some(point(px(600.), px(400.))), false, t2);
        assert!(p.poll(t2 + PEEK_GRACE));
        assert!(!p.peeking(Side::Left));
    }

    #[test]
    fn no_flicker_between_the_zone_and_the_panel() {
        let (mut p, now) = panels(800.);
        p.pointer(near_left(400.), false, now);
        // Sweeping across the gap between edge, zone and panel never schedules
        // a close.
        for x in [0., 9., 10., 11., 12., 40., 270., 281.] {
            p.pointer(Some(point(px(x), px(400.))), false, now);
            assert_eq!(p.next_deadline(), None, "at x={x}");
        }
        // Just past the panel's slack it counts as gone.
        p.pointer(Some(point(px(283.), px(400.))), false, now);
        assert!(p.next_deadline().is_some());
    }

    #[test]
    fn pointer_leaving_the_window_closes_after_grace() {
        let (mut p, now) = panels(800.);
        p.pointer(near_left(400.), false, now);
        p.pointer(None, false, now);
        assert!(p.poll(now + PEEK_GRACE));
    }

    #[test]
    fn held_peeks_stay_until_released() {
        let (mut p, now) = panels(800.);
        p.pointer(near_left(400.), false, now);
        p.set_held(Side::Left, true, now);
        p.pointer(Some(point(px(600.), px(400.))), false, now);
        assert_eq!(p.next_deadline(), None);
        assert!(!p.poll(now + PEEK_GRACE * 10));
        // Released with the pointer away: the grace starts from the release.
        let later = now + PEEK_GRACE * 10;
        p.set_held(Side::Left, false, later);
        assert!(!p.poll(later + PEEK_GRACE / 2));
        assert!(p.poll(later + PEEK_GRACE));
    }

    #[test]
    fn right_edge_peeks_too() {
        let (mut p, now) = panels(1000.);
        assert!(!p.docked(Side::Right));
        assert!(p.pointer(Some(point(px(995.), px(300.))), false, now));
        assert!(p.peeking(Side::Right));
        assert!(!p.peeking(Side::Left), "the left one is docked");
    }

    #[test]
    fn docked_sidebars_ignore_the_edge() {
        let (mut p, now) = panels(1440.);
        assert!(!p.pointer(near_left(400.), false, now));
        assert!(!p.peeking(Side::Left));
    }

    #[test]
    fn toggling_a_peek_docks_it_in_place() {
        let (mut p, now) = panels(800.);
        p.pointer(near_left(400.), false, now);
        let later = now + Duration::from_secs(1);
        p.toggle(Side::Left, later);
        assert_eq!(p.mode(Side::Left), Pinned);
        assert!(p.docked(Side::Left) && !p.peeking(Side::Left));
        let ([left, _], _) = p.frame(later);
        // The panel stays where it is while the column makes room.
        assert_eq!(left.shown, 1.);
        assert!(!left.overlay);
        assert!(left.reserve < 0.01);
        let ([left, _], moving) = p.frame(later + DOCK_SLIDE);
        assert_eq!((left.reserve, moving), (1., false));
    }

    #[test]
    fn frames_slide_and_settle() {
        let (mut p, now) = panels(800.);
        p.pointer(near_left(400.), false, now);
        let ([left, _], moving) = p.frame(now + PEEK_SLIDE / 2);
        assert!(moving && left.overlay && left.shown > 0. && left.shown < 1.);
        assert_eq!(left.reserve, 0.);
        let ([left, _], moving) = p.frame(now + PEEK_SLIDE);
        assert!(!moving && left.shown == 1.);
    }

    #[test]
    fn slide_reverses_from_where_it_was() {
        let start = Instant::now();
        let mut s = Slide::new(true, DOCK_SLIDE);
        s.set(false, start);
        let half = start + DOCK_SLIDE / 2;
        let (at_reverse, _) = s.progress_at(half);
        s.set(true, half);
        let (just_after, _) = s.progress_at(half);
        assert!((at_reverse - just_after).abs() < 1e-6);
        assert_eq!(s.progress_at(half + DOCK_SLIDE * 2), (1., false));
    }
}
