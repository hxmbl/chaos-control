//! Which sidebars are open, and the short slide when one is toggled.
//!
//! This holds the user's intent. The right sidebar is also hidden whenever the
//! window is narrower than the breakpoint (see `workspace.rs`); widening the
//! window brings it back only if the user left it open.

use std::time::{Duration, Instant};

use gpui::{Context, actions};

actions!(chaos_control, [ToggleLeftSidebar, ToggleRightSidebar]);

const SLIDE: Duration = Duration::from_millis(180);

/// One sidebar's open state plus where its slide started.
#[derive(Clone, Copy, Debug)]
struct Slide {
    open: bool,
    /// Progress when the last toggle happened, so reversing mid-slide is smooth.
    from: f32,
    started: Option<Instant>,
}

impl Slide {
    fn new(open: bool) -> Self {
        Self {
            open,
            from: if open { 1. } else { 0. },
            started: None,
        }
    }

    fn toggle(&mut self, now: Instant) {
        self.from = self.progress_at(now).0;
        self.open = !self.open;
        self.started = Some(now);
    }

    /// How shown the sidebar is (0 hidden, 1 shown, eased) and whether it is
    /// still moving.
    fn progress_at(&self, now: Instant) -> (f32, bool) {
        let target = if self.open { 1. } else { 0. };
        let Some(started) = self.started else {
            return (target, false);
        };
        let t =
            (now.saturating_duration_since(started).as_secs_f32() / SLIDE.as_secs_f32()).min(1.);
        let eased = 1. - (1. - t).powi(3);
        (self.from + (target - self.from) * eased, t < 1.)
    }
}

pub struct Panels {
    left: Slide,
    right: Slide,
}

impl Panels {
    pub fn new() -> Self {
        Self {
            left: Slide::new(true),
            right: Slide::new(true),
        }
    }

    pub fn left_open(&self) -> bool {
        self.left.open
    }

    pub fn right_open(&self) -> bool {
        self.right.open
    }

    pub fn toggle_left(&mut self, cx: &mut Context<Self>) {
        self.left.toggle(Instant::now());
        cx.notify();
    }

    pub fn toggle_right(&mut self, cx: &mut Context<Self>) {
        self.right.toggle(Instant::now());
        cx.notify();
    }

    /// (left, right) progress, and whether either is still sliding.
    pub fn progress(&self) -> (f32, f32, bool) {
        let now = Instant::now();
        let (l, l_moving) = self.left.progress_at(now);
        let (r, r_moving) = self.right.progress_at(now);
        (l, r, l_moving || r_moving)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slide_settles_at_its_target() {
        let start = Instant::now();
        let mut s = Slide::new(true);
        assert_eq!(s.progress_at(start), (1., false));
        s.toggle(start);
        let (mid, moving) = s.progress_at(start + SLIDE / 2);
        assert!(moving && mid > 0. && mid < 1.);
        assert_eq!(s.progress_at(start + SLIDE * 2), (0., false));
    }

    #[test]
    fn reversing_mid_slide_starts_where_it_was() {
        let start = Instant::now();
        let mut s = Slide::new(true);
        s.toggle(start);
        let half = start + SLIDE / 2;
        let (at_reverse, _) = s.progress_at(half);
        s.toggle(half);
        let (just_after, _) = s.progress_at(half);
        assert!((at_reverse - just_after).abs() < 1e-6);
        assert_eq!(s.progress_at(half + SLIDE * 2), (1., false));
    }
}
