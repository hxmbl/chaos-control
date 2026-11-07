//! Where the caret is drawn while it moves.
//!
//! The caret is not snapped to its new offset when the cursor moves. Four
//! corners, one damped spring each, chase the target: short hops ease quickly,
//! long jumps rank the leading corners ahead of the trailing ones, and the
//! result is a bar that slides across and briefly trails behind itself.
//!
//! Adapted from Zed's editor cursor animation
//! (`crates/editor/src/cursor_animation.rs`), which in turn adapts
//! vscode-neovide-cursor by LengineerC (MIT). A single-line field only moves in
//! x, so the vertical springs and the per-selection map are gone; the physics
//! and the thresholds are unchanged.

use std::time::{Duration, Instant};

use gpui::{Bounds, Pixels, Point, point, px};

const MAX_FRAME_DURATION: Duration = Duration::from_millis(33);
const SPRING_RESET_EPSILON: f32 = 0.001;
const SPRING_ACTIVE_EPSILON: f32 = 0.01;
const CORNER_ACTIVE_DISTANCE: f32 = 0.5;
const GEOMETRY_EPSILON: f32 = 0.01;
const SHORT_MOVE_THRESHOLD: f32 = 8.0;
const LEADING_SNAP_THRESHOLD: f32 = 0.5;
const SNAP_ANIMATION_LENGTH_SECONDS: f32 = 0.02;
const ANIMATION_RESET_THRESHOLD_SECONDS: f32 = 0.075;
const MAX_TRAIL_DISTANCE_FACTOR: f32 = 100.0;
const RANK_TRAIL_FACTORS: [f32; 4] = [1.0, 0.9, 0.5, 0.3];
const ANIMATION_LENGTH_SECONDS: f32 = 0.125;
const SHORT_ANIMATION_LENGTH_SECONDS: f32 = 0.05;

/// The parts of the field's geometry used to decide whether the caret should
/// animate this frame or snap. `origin_x` is where the shaped line starts, so it
/// moves when the text scrolls; the rest only moves on a resize, which snaps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CursorViewport {
    origin_x: Pixels,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
}

impl CursorViewport {
    pub(crate) fn new(origin_x: Pixels, bounds: Bounds<Pixels>, line_height: Pixels) -> Self {
        Self {
            origin_x,
            bounds,
            line_height,
        }
    }

    fn is_finite(self) -> bool {
        f32::from(self.origin_x).is_finite()
            && f32::from(self.bounds.origin.x).is_finite()
            && f32::from(self.bounds.origin.y).is_finite()
            && f32::from(self.bounds.size.width).is_finite()
            && f32::from(self.bounds.size.height).is_finite()
            && f32::from(self.line_height).is_finite()
    }

    fn has_same_layout(self, other: Self) -> bool {
        self.bounds == other.bounds && self.line_height == other.line_height
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    origin_x: f32,
    origin_y: f32,
    width: f32,
    height: f32,
}

impl Geometry {
    fn from_bounds(bounds: Bounds<Pixels>) -> Self {
        Self {
            origin_x: bounds.origin.x.into(),
            origin_y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
        }
    }

    fn center_x(self) -> f32 {
        self.origin_x + self.width / 2.0
    }

    fn is_finite(self) -> bool {
        self.origin_x.is_finite()
            && self.origin_y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 0.0
            && self.height > 0.0
    }

    fn has_same_size(self, other: Self) -> bool {
        nearly_equal(self.width, other.width) && nearly_equal(self.height, other.height)
    }

    fn has_same_origin(self, other: Self) -> bool {
        nearly_equal(self.origin_x, other.origin_x) && nearly_equal(self.origin_y, other.origin_y)
    }
}

/// One axis of a damped spring, from the reference implementation. `position`
/// is the distance still to travel.
#[derive(Clone, Copy, Debug, Default)]
struct DampedSpring {
    position: f32,
    velocity: f32,
}

impl DampedSpring {
    fn update(&mut self, elapsed_seconds: f32, animation_length: f32) {
        if !elapsed_seconds.is_finite()
            || elapsed_seconds < 0.0
            || !animation_length.is_finite()
            || animation_length <= elapsed_seconds
            || self.position.abs() < SPRING_RESET_EPSILON
        {
            self.reset();
            return;
        }

        if elapsed_seconds == 0.0 {
            // Nothing moves this frame, but a spring already at rest should not
            // keep asking for frames.
            if self.position.abs() < SPRING_ACTIVE_EPSILON {
                self.reset();
            }
            return;
        }

        let angular_frequency = 4.0 / animation_length;
        let initial_position = self.position;
        let combined_velocity = self.position * angular_frequency + self.velocity;
        let decay = (-angular_frequency * elapsed_seconds).exp();
        self.position = (initial_position + combined_velocity * elapsed_seconds) * decay;
        self.velocity = decay
            * (-initial_position * angular_frequency
                - combined_velocity * elapsed_seconds * angular_frequency
                + combined_velocity);

        if !self.position.is_finite()
            || !self.velocity.is_finite()
            || self.position.abs() < SPRING_RESET_EPSILON
        {
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.position = 0.0;
        self.velocity = 0.0;
    }
}

#[derive(Clone, Copy, Debug)]
struct Corner {
    /// -0.5 for the left edge of the bar, 0.5 for the right.
    relative_x: f32,
    /// Whether this corner rides the top or the bottom of the bar. The bar never
    /// moves vertically, so the corners only need to know where to be drawn.
    top: bool,
    current_x: f32,
    target_x: f32,
    horizontal: DampedSpring,
    animation_length: f32,
}

impl Corner {
    fn new(relative_x: f32, top: bool) -> Self {
        Self {
            relative_x,
            top,
            current_x: 0.0,
            target_x: 0.0,
            horizontal: DampedSpring::default(),
            animation_length: 0.0,
        }
    }

    fn destination_x(self, geometry: Geometry) -> f32 {
        geometry.center_x() + self.relative_x * geometry.width
    }

    /// How much of the movement points the same way as this corner: 1 for a
    /// leading corner, -1 for a trailing one.
    fn direction_alignment(self, geometry: Geometry) -> f32 {
        let travel = self.destination_x(geometry) - self.current_x;
        if nearly_equal(travel, 0.0) {
            0.0
        } else {
            travel.signum() * self.relative_x.signum()
        }
    }

    fn snap(&mut self, geometry: Geometry) {
        let destination = self.destination_x(geometry);
        self.current_x = destination;
        self.target_x = destination;
        self.horizontal.reset();
    }

    fn retarget(&mut self, geometry: Geometry, rank: usize) {
        let destination = self.destination_x(geometry);
        // Jumps are measured in cursor widths, as in the reference. The caret's
        // own width is small, so most typed characters fall under the short-hop
        // threshold and only word jumps get the full-length animation.
        let horizontal_jump = (destination - self.target_x) / geometry.width.max(f32::EPSILON);
        let is_short_jump = horizontal_jump.abs() <= SHORT_MOVE_THRESHOLD;
        let base_animation_length = if is_short_jump {
            ANIMATION_LENGTH_SECONDS.min(SHORT_ANIMATION_LENGTH_SECONDS)
        } else {
            ANIMATION_LENGTH_SECONDS
        };
        let leading_alignment = if nearly_equal(horizontal_jump, 0.0) {
            0.0
        } else {
            horizontal_jump.signum() * self.relative_x.signum()
        };
        // The leading edge snaps almost at once; the trailing corners keep the
        // momentum and stretch the bar behind it.
        self.animation_length = if leading_alignment > LEADING_SNAP_THRESHOLD {
            SNAP_ANIMATION_LENGTH_SECONDS
        } else {
            base_animation_length * RANK_TRAIL_FACTORS[rank.min(3)]
        };

        if self.animation_length > ANIMATION_RESET_THRESHOLD_SECONDS {
            self.horizontal.reset();
        }

        self.target_x = destination;
        self.horizontal.position = destination - self.current_x;
    }

    /// Advances the corner and reports whether it still needs frames.
    fn update(&mut self, elapsed_seconds: f32, max_trail_distance: f32) -> bool {
        self.horizontal
            .update(elapsed_seconds, self.animation_length);
        self.horizontal.position = self
            .horizontal
            .position
            .clamp(-max_trail_distance, max_trail_distance);
        self.current_x = self.target_x - self.horizontal.position;
        self.horizontal.position.abs() > CORNER_ACTIVE_DISTANCE
    }
}

/// The caret's animation state for one text field.
#[derive(Clone, Debug)]
pub(crate) struct CursorAnimation {
    corners: [Corner; 4],
    target_geometry: Option<Geometry>,
    last_offset: Option<usize>,
    last_viewport: Option<CursorViewport>,
    last_frame_at: Option<Instant>,
    active: bool,
}

impl Default for CursorAnimation {
    fn default() -> Self {
        // Left top, right top, right bottom, left bottom.
        Self {
            corners: [
                Corner::new(-0.5, true),
                Corner::new(0.5, true),
                Corner::new(0.5, false),
                Corner::new(-0.5, false),
            ],
            target_geometry: None,
            last_offset: None,
            last_viewport: None,
            last_frame_at: None,
            active: false,
        }
    }
}

impl CursorAnimation {
    /// Feeds this frame's caret geometry in. Returns the corners to paint while
    /// the caret is still moving, or `None` once it has settled (paint the
    /// static caret then).
    pub(crate) fn update(
        &mut self,
        offset: usize,
        target_bounds: Bounds<Pixels>,
        viewport: CursorViewport,
        now: Instant,
    ) -> Option<[Point<Pixels>; 4]> {
        let target_geometry = Geometry::from_bounds(target_bounds);
        if !target_geometry.is_finite() || !viewport.is_finite() {
            self.reset();
            return None;
        }

        let Some(mut previous_geometry) = self.target_geometry else {
            self.snap(offset, target_geometry, viewport, now);
            return None;
        };
        let Some(previous_viewport) = self.last_viewport else {
            self.snap(offset, target_geometry, viewport, now);
            return None;
        };

        let offset_changed = self.last_offset != Some(offset);
        let viewport_changed = previous_viewport != viewport;

        if viewport_changed {
            if !previous_viewport.has_same_layout(viewport) {
                // A resize (or the field moving) invalidates the corner
                // positions; there is nothing meaningful to animate from.
                self.snap(offset, target_geometry, viewport, now);
                return None;
            }

            // Only the text scrolled. Keep the corners anchored to the glyphs
            // they were chasing by moving them with the content.
            self.translate(viewport.origin_x - previous_viewport.origin_x);
            if let Some(target_geometry) = self.target_geometry {
                previous_geometry = target_geometry;
            }
        }

        let target_origin_changed = !previous_geometry.has_same_origin(target_geometry);
        let geometry_size_changed = !previous_geometry.has_same_size(target_geometry);
        let geometry_changed = target_origin_changed || geometry_size_changed;

        if !offset_changed && geometry_changed {
            // The caret did not move but its geometry did (a relayout): snap
            // rather than slide to a position the user did not ask for.
            self.snap(offset, target_geometry, viewport, now);
            return None;
        }

        self.last_offset = Some(offset);
        self.last_viewport = Some(viewport);
        self.target_geometry = Some(target_geometry);

        if offset_changed && geometry_changed {
            let elapsed = if self.active {
                self.elapsed_since_last_frame(now)
            } else {
                Duration::ZERO
            };
            self.retarget(target_geometry);
            self.advance(elapsed);
            self.last_frame_at = Some(now);
        } else if self.active {
            let elapsed = self.elapsed_since_last_frame(now);
            self.advance(elapsed);
            self.last_frame_at = Some(now);
        }

        self.active.then(|| {
            self.corners.map(|corner| {
                let y = if corner.top {
                    target_geometry.origin_y
                } else {
                    target_geometry.origin_y + target_geometry.height
                };
                point(px(corner.current_x), px(y))
            })
        })
    }

    pub(crate) fn reset(&mut self) {
        self.target_geometry = None;
        self.last_offset = None;
        self.last_viewport = None;
        self.last_frame_at = None;
        self.active = false;
        for corner in &mut self.corners {
            corner.horizontal.reset();
        }
    }

    #[cfg(test)]
    fn is_active(&self) -> bool {
        self.active
    }

    fn translate(&mut self, delta: Pixels) {
        let delta: f32 = delta.into();
        for corner in &mut self.corners {
            corner.current_x += delta;
            corner.target_x += delta;
        }
        if let Some(target_geometry) = &mut self.target_geometry {
            target_geometry.origin_x += delta;
        }
    }

    fn snap(&mut self, offset: usize, geometry: Geometry, viewport: CursorViewport, now: Instant) {
        for corner in &mut self.corners {
            corner.snap(geometry);
        }
        self.target_geometry = Some(geometry);
        self.last_offset = Some(offset);
        self.last_viewport = Some(viewport);
        self.last_frame_at = Some(now);
        self.active = false;
    }

    fn retarget(&mut self, geometry: Geometry) {
        let mut aligned_corners: [(usize, f32); 4] =
            std::array::from_fn(|index| (index, self.corners[index].direction_alignment(geometry)));
        aligned_corners.sort_by(|left, right| left.1.total_cmp(&right.1));
        let mut ranks = [0; 4];
        for (rank, (index, _)) in aligned_corners.into_iter().enumerate() {
            ranks[index] = rank;
        }

        for (index, corner) in self.corners.iter_mut().enumerate() {
            corner.retarget(geometry, ranks[index]);
        }
        self.active = self
            .corners
            .iter()
            .any(|corner| corner.horizontal.position != 0.0);
    }

    fn elapsed_since_last_frame(&self, now: Instant) -> Duration {
        self.last_frame_at
            .and_then(|last_frame_at| now.checked_duration_since(last_frame_at))
            .unwrap_or_default()
            .min(MAX_FRAME_DURATION)
    }

    fn advance(&mut self, elapsed: Duration) {
        let elapsed_seconds = elapsed.as_secs_f32();
        let max_trail_distance = self
            .target_geometry
            .map(|geometry| geometry.width.max(geometry.height) * MAX_TRAIL_DISTANCE_FACTOR)
            .unwrap_or_default();
        let mut active = false;
        for corner in &mut self.corners {
            // Advance every corner: `|| active` must not short-circuit the update.
            active = corner.update(elapsed_seconds, max_trail_distance) || active;
        }
        self.active = active;

        if !self.active
            && let Some(target_geometry) = self.target_geometry
        {
            for corner in &mut self.corners {
                corner.snap(target_geometry);
            }
        }
    }
}

fn nearly_equal(left: f32, right: f32) -> bool {
    (left - right).abs() <= GEOMETRY_EPSILON
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    fn bounds(x: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(0.)),
            size: size(px(1.5), px(20.)),
        }
    }

    fn viewport(origin_x: f32) -> CursorViewport {
        CursorViewport::new(
            px(origin_x),
            Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(400.), px(20.)),
            },
            px(20.),
        )
    }

    fn snapped_at(x: f32) -> (CursorAnimation, Instant) {
        let now = Instant::now();
        let mut state = CursorAnimation::default();
        assert!(
            state.update(0, bounds(x), viewport(0.), now).is_none(),
            "the first frame has nothing to animate from"
        );
        (state, now)
    }

    #[test]
    fn spring_converges() {
        let mut spring = DampedSpring {
            position: 100.0,
            velocity: 0.0,
        };

        for _ in 0..60 {
            spring.update(1.0 / 120.0, 0.1);
        }

        assert_eq!(spring.position, 0.0);
        assert_eq!(spring.velocity, 0.0);
    }

    #[test]
    fn first_position_snaps_and_reset_forgets_it() {
        let (mut state, now) = snapped_at(10.0);
        assert!(!state.is_active());
        assert_eq!(state.corners[0].current_x, 10.0);

        state.reset();
        assert!(state.target_geometry.is_none());
        assert!(state.last_offset.is_none());
        assert!(
            state.update(3, bounds(20.0), viewport(0.), now).is_none(),
            "after a reset the caret snaps to wherever it is now"
        );
    }

    #[test]
    fn moving_the_caret_animates_and_converges() {
        let (mut state, now) = snapped_at(0.0);

        let corners = state
            .update(1, bounds(100.0), viewport(0.), now)
            .expect("a far move animates");
        assert!(state.is_active());
        assert_eq!(corners[0].x, px(0.0), "the animation starts where it was");

        for frame in 1..40 {
            state.update(
                1,
                bounds(100.0),
                viewport(0.),
                now + Duration::from_millis(frame * 16),
            );
        }

        assert!(!state.is_active());
        assert_eq!(state.corners[0].current_x, 100.0);
        assert_eq!(state.corners[1].current_x, 101.5);
    }

    #[test]
    fn short_horizontal_movement_uses_short_duration() {
        let (mut state, now) = snapped_at(0.0);
        state.update(1, bounds(4.0), viewport(0.), now);

        assert!((state.corners[0].animation_length - 0.05).abs() < f32::EPSILON);
        assert!((state.corners[3].animation_length - 0.045).abs() < f32::EPSILON);
        assert!((state.corners[1].animation_length - 0.02).abs() < f32::EPSILON);
        assert!((state.corners[2].animation_length - 0.02).abs() < f32::EPSILON);
    }

    #[test]
    fn long_movement_ranks_leading_corners_ahead_of_trailing_corners() {
        let (mut state, now) = snapped_at(0.0);
        state.update(1, bounds(100.0), viewport(0.), now);

        assert!(state.corners[1].animation_length < state.corners[0].animation_length);
        assert!(state.corners[2].animation_length < state.corners[3].animation_length);
    }

    #[test]
    fn geometry_change_without_movement_snaps() {
        let (mut state, now) = snapped_at(0.0);

        assert!(
            state
                .update(
                    0,
                    bounds(4.0),
                    viewport(0.),
                    now + Duration::from_millis(16)
                )
                .is_none()
        );
        assert!(!state.is_active());
        assert_eq!(state.corners[0].current_x, 4.0);
        assert_eq!(state.corners[1].current_x, 5.5);
    }

    #[test]
    fn scrolling_during_animation_preserves_motion_and_velocity() {
        let (mut state, now) = snapped_at(0.0);
        state.update(1, bounds(100.0), viewport(0.), now);
        state.update(
            1,
            bounds(100.0),
            viewport(0.),
            now + Duration::from_millis(16),
        );
        assert!(state.is_active());
        let positions_before_scroll = state.corners.map(|corner| corner.current_x);
        let velocities_before_scroll = state.corners.map(|corner| corner.horizontal.velocity);

        let corners = state
            .update(
                1,
                // The text scrolled left by 20, taking the caret with it.
                bounds(80.0),
                viewport(-20.),
                now + Duration::from_millis(16),
            )
            .expect("scrolling does not cancel the animation");
        assert!(state.is_active());
        assert_eq!(
            state.corners.map(|corner| corner.current_x),
            positions_before_scroll.map(|position| position - 20.0)
        );
        assert_eq!(
            state.corners.map(|corner| corner.horizontal.velocity),
            velocities_before_scroll
        );
        assert_eq!(corners[0].x, px(positions_before_scroll[0] - 20.0));
    }

    #[test]
    fn layout_changes_snap_even_during_animation() {
        let now = Instant::now();
        let changed_viewport = CursorViewport::new(
            px(0.),
            Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(300.), px(20.)),
            },
            px(20.),
        );

        let mut state = CursorAnimation::default();
        state.update(0, bounds(0.0), viewport(0.), now);
        state.update(1, bounds(100.0), viewport(0.), now);
        assert!(state.is_active());

        assert!(
            state
                .update(
                    1,
                    bounds(100.0),
                    changed_viewport,
                    now + Duration::from_millis(16),
                )
                .is_none()
        );
        assert!(!state.is_active());
        assert_eq!(state.corners[0].current_x, 100.0);
    }

    #[test]
    fn invalid_or_large_elapsed_time_never_produces_non_finite_values() {
        let mut spring = DampedSpring {
            position: 100.0,
            velocity: 10.0,
        };
        spring.update(f32::NAN, 0.1);
        assert!(spring.position.is_finite());
        assert!(spring.velocity.is_finite());

        spring.position = f32::INFINITY;
        spring.velocity = f32::NEG_INFINITY;
        spring.update(1.0, 0.1);
        assert!(spring.position.is_finite());
        assert!(spring.velocity.is_finite());
    }

    #[test]
    fn moving_after_idling_starts_from_the_previous_geometry() {
        let (mut state, now) = snapped_at(0.0);

        let corners = state
            .update(1, bounds(4.0), viewport(0.), now + Duration::from_secs(5))
            .expect("the move animates");
        assert_eq!(corners[0].x, px(0.0));

        let corners = state
            .update(
                1,
                bounds(4.0),
                viewport(0.),
                now + Duration::from_secs(5) + Duration::from_millis(16),
            )
            .expect("still moving");
        assert!(corners[0].x > px(0.0));
        assert!(corners[0].x < px(4.0));
    }
}
