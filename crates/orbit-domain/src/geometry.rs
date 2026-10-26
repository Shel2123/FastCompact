//! Radial menu geometry and sector hit testing.
//!
//! Drawing and input use the same calculation: [`MenuGeometry::slot_bounds`]
//! describes what is drawn, and [`MenuGeometry::hit`] decides what is
//! chosen. Angles are in radians, measured clockwise from straight up.
//!
//! Conversions from Quartz coordinates (origin top left) and into view
//! coordinates belong to the platform adapter; here every point is already in
//! Cocoa screen space.

use std::f64::consts::{PI, TAU};

use thiserror::Error;

/// A point in global Cocoa screen coordinates: origin at the bottom left of
/// the primary display, y grows upwards, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenPoint {
    /// Horizontal coordinate.
    pub x: f64,
    /// Vertical coordinate, growing upwards.
    pub y: f64,
}

impl ScreenPoint {
    /// A point at `x`, `y`.
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// This point as an offset from `center`.
    pub fn relative_to(self, center: Self) -> MenuPoint {
        MenuPoint {
            x: self.x - center.x,
            y: self.y - center.y,
        }
    }

    fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// An offset from the menu center, in points, y growing upwards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuPoint {
    /// Horizontal offset.
    pub x: f64,
    /// Vertical offset, growing upwards.
    pub y: f64,
}

impl MenuPoint {
    /// Distance from the menu center.
    pub fn length(self) -> f64 {
        self.x.hypot(self.y)
    }

    /// Clockwise angle from straight up, in `[0, 2π)`.
    pub fn angle(self) -> f64 {
        self.x.atan2(self.y).rem_euclid(TAU)
    }
}

/// A screen area in Cocoa screen coordinates, such as the visible frame of a
/// display (without the menu bar and the Dock).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    /// Bottom-left corner.
    pub origin: ScreenPoint,
    /// Width in points.
    pub width: f64,
    /// Height in points.
    pub height: f64,
}

impl ScreenRect {
    /// A rectangle with its bottom-left corner at `x`, `y`.
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            origin: ScreenPoint::new(x, y),
            width,
            height,
        }
    }

    fn max_x(&self) -> f64 {
        self.origin.x + self.width
    }

    fn max_y(&self) -> f64 {
        self.origin.y + self.height
    }

    /// Half-open, so a point on the edge between two adjacent displays
    /// belongs to exactly one of them.
    fn contains(&self, point: ScreenPoint) -> bool {
        (self.origin.x..self.max_x()).contains(&point.x)
            && (self.origin.y..self.max_y()).contains(&point.y)
    }

    fn distance_to(&self, point: ScreenPoint) -> f64 {
        let dx = (self.origin.x - point.x)
            .max(point.x - self.max_x())
            .max(0.0);
        let dy = (self.origin.y - point.y)
            .max(point.y - self.max_y())
            .max(0.0);
        dx.hypot(dy)
    }
}

/// What a cursor position chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hit {
    /// Nothing: the cursor has not left the invocation point, or it is in
    /// the neutral zone at the center.
    Neutral,
    /// The slot with this index.
    Slot(usize),
    /// Nothing: the cursor is beyond the menu.
    Outside,
}

/// Why a [`MenuGeometry`] cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Error)]
pub enum GeometryError {
    /// A ring needs at least two slots.
    #[error("the menu needs at least two slots")]
    TooFewSlots,
    /// Radii and the movement threshold must be finite and positive, and the
    /// rotation finite.
    #[error("menu sizes must be finite and positive")]
    InvalidSize,
    /// The neutral zone must be smaller than the menu.
    #[error("the neutral zone must be smaller than the menu")]
    NeutralNotInside,
}

/// A ring of equal slots around a neutral zone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuGeometry {
    slots: usize,
    neutral_radius: f64,
    outer_radius: f64,
    move_threshold: f64,
    rotation: f64,
}

impl MenuGeometry {
    /// A ring of `slots` slots. Slot 0 is centered at `rotation`, the others
    /// follow clockwise.
    ///
    /// # Errors
    ///
    /// See [`GeometryError`].
    pub fn new(
        slots: usize,
        neutral_radius: f64,
        outer_radius: f64,
        move_threshold: f64,
        rotation: f64,
    ) -> Result<Self, GeometryError> {
        if slots < 2 {
            return Err(GeometryError::TooFewSlots);
        }
        let positive = [neutral_radius, outer_radius, move_threshold]
            .iter()
            .all(|value| value.is_finite() && *value > 0.0);
        if !positive || !rotation.is_finite() {
            return Err(GeometryError::InvalidSize);
        }
        if neutral_radius >= outer_radius {
            return Err(GeometryError::NeutralNotInside);
        }
        Ok(Self {
            slots,
            neutral_radius,
            outer_radius,
            move_threshold,
            rotation: rotation.rem_euclid(TAU),
        })
    }

    /// The menu of [`MENU_LAYOUT`](crate::action::MENU_LAYOUT): nine slots,
    /// turned so that [`CANCEL_SLOT`](crate::action::CANCEL_SLOT) points
    /// straight down. The radii are a first guess, tuned with the real menu.
    pub fn standard() -> Self {
        const SLOTS: usize = crate::action::MENU_LAYOUT.len();
        // Cancel centered at π: rotation + CANCEL_SLOT × step = π. Slot
        // counts and indices are tiny, so the casts are exact.
        let rotation = PI - crate::action::CANCEL_SLOT as f64 * TAU / SLOTS as f64;
        Self {
            slots: SLOTS,
            neutral_radius: 28.0,
            outer_radius: 110.0,
            move_threshold: 6.0,
            rotation: rotation.rem_euclid(TAU),
        }
    }

    /// Number of slots.
    pub const fn slots(&self) -> usize {
        self.slots
    }

    /// Radius of the neutral zone at the center.
    pub const fn neutral_radius(&self) -> f64 {
        self.neutral_radius
    }

    /// Outer radius of the ring.
    pub const fn outer_radius(&self) -> f64 {
        self.outer_radius
    }

    /// How far the cursor must move from the invocation point before
    /// anything can be chosen.
    pub const fn move_threshold(&self) -> f64 {
        self.move_threshold
    }

    fn step(&self) -> f64 {
        TAU / self.slots as f64
    }

    /// Clockwise angle from straight up to the middle of `slot`, in
    /// `[0, 2π)`.
    pub fn slot_center(&self, slot: usize) -> f64 {
        (self.rotation + slot as f64 * self.step()).rem_euclid(TAU)
    }

    /// Start and end angles of `slot`, clockwise. The start is in
    /// `[0, 2π)` and belongs to the slot; the end is the start plus one slot
    /// width and belongs to the next slot. For the slot that crosses straight
    /// up, the end is above 2π.
    pub fn slot_bounds(&self, slot: usize) -> (f64, f64) {
        let start = (self.slot_center(slot) - self.step() / 2.0).rem_euclid(TAU);
        (start, start + self.step())
    }

    /// What the cursor chooses in a menu centered at `center` that was
    /// invoked at `invoke`.
    ///
    /// Until the cursor moves at least [`MenuGeometry::move_threshold`] away
    /// from `invoke`, nothing is chosen, even when the menu was shifted away
    /// from a screen edge and `invoke` lies over a slot (I3). After that the
    /// position relative to `center` decides. The result depends only on the
    /// current points, so the highlight and the final choice always agree.
    pub fn hit(&self, center: ScreenPoint, invoke: ScreenPoint, cursor: ScreenPoint) -> Hit {
        if !(center.is_finite() && invoke.is_finite() && cursor.is_finite()) {
            return Hit::Outside;
        }
        if cursor.relative_to(invoke).length() < self.move_threshold {
            return Hit::Neutral;
        }
        let offset = cursor.relative_to(center);
        let distance = offset.length();
        if distance < self.neutral_radius {
            Hit::Neutral
        } else if distance > self.outer_radius {
            Hit::Outside
        } else {
            Hit::Slot(self.slot_at(offset.angle()))
        }
    }

    /// Slot under a clockwise angle from straight up. An angle on an edge
    /// belongs to the slot that starts there. Slot edges are irrational
    /// angles, so a position within [`EDGE_TOLERANCE`] of an edge counts as
    /// the edge; otherwise rounding noise would decide.
    fn slot_at(&self, angle: f64) -> usize {
        let step = self.step();
        let from_start = (angle - self.rotation + step / 2.0).rem_euclid(TAU);
        let position = from_start / step;
        let edge = position.round();
        let position = if (position - edge).abs() < EDGE_TOLERANCE {
            edge
        } else {
            position.floor()
        };
        // `position` is in [0, slots]; slots itself is the edge at the start
        // of slot 0 again.
        position as usize % self.slots
    }
}

/// Fraction of a slot width within which an angle counts as lying on a slot
/// edge.
const EDGE_TOLERANCE: f64 = 1e-9;

/// Center for a menu of `radius` invoked at `invoke`, shifted so the whole
/// menu stays on the display under `invoke`.
///
/// The display is the one that contains `invoke`, or the nearest one if none
/// does. On a display narrower or lower than the menu, the menu is centered
/// on that axis. Returns `None` without displays or for a non-finite point.
pub fn place_menu(invoke: ScreenPoint, screens: &[ScreenRect], radius: f64) -> Option<ScreenPoint> {
    if !invoke.is_finite() {
        return None;
    }
    let screen = screens
        .iter()
        .find(|screen| screen.contains(invoke))
        .or_else(|| {
            screens
                .iter()
                .min_by(|a, b| a.distance_to(invoke).total_cmp(&b.distance_to(invoke)))
        })?;
    Some(ScreenPoint::new(
        clamp_axis(invoke.x, screen.origin.x, screen.max_x(), radius),
        clamp_axis(invoke.y, screen.origin.y, screen.max_y(), radius),
    ))
}

fn clamp_axis(value: f64, min: f64, max: f64, radius: f64) -> f64 {
    if max - min < 2.0 * radius {
        f64::midpoint(min, max)
    } else {
        value.clamp(min + radius, max - radius)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{CANCEL_SLOT, MENU_LAYOUT};

    const PRIMARY: ScreenRect = ScreenRect::new(0.0, 0.0, 1440.0, 875.0);
    /// To the left of the primary display, lower and larger.
    const LEFT: ScreenRect = ScreenRect::new(-1920.0, -300.0, 1920.0, 1080.0);
    /// Above the primary display.
    const ABOVE: ScreenRect = ScreenRect::new(100.0, 900.0, 1280.0, 800.0);
    const SCREENS: [ScreenRect; 3] = [PRIMARY, LEFT, ABOVE];

    fn standard() -> MenuGeometry {
        MenuGeometry::standard()
    }

    /// A point at `distance` from `center`, at a clockwise angle from up.
    fn polar(center: ScreenPoint, angle: f64, distance: f64) -> ScreenPoint {
        ScreenPoint::new(
            center.x + distance * angle.sin(),
            center.y + distance * angle.cos(),
        )
    }

    fn place(invoke: ScreenPoint) -> ScreenPoint {
        place_menu(invoke, &SCREENS, standard().outer_radius())
            .unwrap_or_else(|| panic!("no placement for {invoke:?}"))
    }

    /// Center, edge midpoints and corners of a display, just inside it.
    fn probe_points(screen: ScreenRect) -> Vec<ScreenPoint> {
        let (min_x, min_y) = (screen.origin.x, screen.origin.y);
        let (max_x, max_y) = (screen.max_x() - 0.5, screen.max_y() - 0.5);
        let (mid_x, mid_y) = (f64::midpoint(min_x, max_x), f64::midpoint(min_y, max_y));
        vec![
            ScreenPoint::new(mid_x, mid_y),
            ScreenPoint::new(min_x, mid_y),
            ScreenPoint::new(max_x, mid_y),
            ScreenPoint::new(mid_x, min_y),
            ScreenPoint::new(mid_x, max_y),
            ScreenPoint::new(min_x, min_y),
            ScreenPoint::new(min_x, max_y),
            ScreenPoint::new(max_x, min_y),
            ScreenPoint::new(max_x, max_y),
        ]
    }

    #[test]
    fn press_and_release_without_movement_chooses_nothing() {
        let geometry = standard();
        for screen in SCREENS {
            for invoke in probe_points(screen) {
                let center = place(invoke);
                assert_eq!(
                    geometry.hit(center, invoke, invoke),
                    Hit::Neutral,
                    "invoke {invoke:?}, center {center:?}"
                );
            }
        }
    }

    #[test]
    fn shifted_menu_needs_movement_before_a_slot_is_chosen() {
        let geometry = standard();
        // Near the left edge: the menu moves right, and the invocation point
        // ends up over the slot that points left.
        let invoke = ScreenPoint::new(5.0, 400.0);
        let center = place(invoke);
        assert_eq!(center, ScreenPoint::new(110.0, 400.0));
        let left_slot = geometry.slot_at(3.0 * PI / 2.0);
        let under_invoke = invoke.relative_to(center);
        assert!(under_invoke.length() > geometry.neutral_radius());
        assert!(under_invoke.length() <= geometry.outer_radius());

        assert_eq!(geometry.hit(center, invoke, invoke), Hit::Neutral);
        let small = ScreenPoint::new(invoke.x + 3.0, invoke.y + 2.0);
        assert_eq!(geometry.hit(center, invoke, small), Hit::Neutral);
        let moved = ScreenPoint::new(invoke.x + 20.0, invoke.y);
        assert_eq!(geometry.hit(center, invoke, moved), Hit::Slot(left_slot));
    }

    #[test]
    fn corner_invocation_outside_the_shifted_ring() {
        let geometry = standard();
        let invoke = ScreenPoint::new(1.0, 1.0);
        let center = place(invoke);
        assert_eq!(center, ScreenPoint::new(110.0, 110.0));
        assert!(invoke.relative_to(center).length() > geometry.outer_radius());

        // Not moved: neutral, although the point is beyond the ring.
        assert_eq!(geometry.hit(center, invoke, invoke), Hit::Neutral);
        // Moved past the threshold but still beyond the ring.
        let moved = ScreenPoint::new(invoke.x + 8.0, invoke.y);
        assert_eq!(geometry.hit(center, invoke, moved), Hit::Outside);
        // Into the ring, straight below the center: Cancel.
        let below = polar(center, PI, 60.0);
        assert_eq!(geometry.hit(center, invoke, below), Hit::Slot(CANCEL_SLOT));
    }

    #[test]
    fn every_slot_center_hits_its_slot() {
        let geometry = standard();
        assert_eq!(geometry.slots(), MENU_LAYOUT.len());
        let center = ScreenPoint::new(500.0, 500.0);
        let middle = f64::midpoint(geometry.neutral_radius(), geometry.outer_radius());
        for slot in 0..geometry.slots() {
            let cursor = polar(center, geometry.slot_center(slot), middle);
            assert_eq!(
                geometry.hit(center, center, cursor),
                Hit::Slot(slot),
                "slot {slot}"
            );
        }
    }

    #[test]
    fn cancel_points_straight_down_and_up_is_a_boundary() {
        let geometry = standard();
        assert!((geometry.slot_center(CANCEL_SLOT) - PI).abs() < 1e-12);
        let center = ScreenPoint::new(0.0, 0.0);
        let down = ScreenPoint::new(0.0, -60.0);
        assert_eq!(geometry.hit(center, center, down), Hit::Slot(CANCEL_SLOT));
        // Straight up is the edge between the last slot and slot 0; the edge
        // belongs to the slot that follows clockwise.
        let up = ScreenPoint::new(0.0, 60.0);
        assert_eq!(geometry.hit(center, center, up), Hit::Slot(0));
        let (start, end) = geometry.slot_bounds(geometry.slots() - 1);
        assert!(start < TAU && end > start);
        assert!((end - TAU).abs() < 1e-12, "the last slot ends straight up");
        let (start, _) = geometry.slot_bounds(0);
        assert!(start.abs() < 1e-12 || (TAU - start).abs() < 1e-12);
    }

    #[test]
    fn angle_boundaries_belong_to_the_next_slot_clockwise() {
        // Four slots centered up, right, down, left: edges at the diagonals.
        let geometry =
            MenuGeometry::new(4, 10.0, 100.0, 1.0, 0.0).unwrap_or_else(|err| panic!("{err}"));
        let center = ScreenPoint::new(0.0, 0.0);
        let cases = [
            (ScreenPoint::new(30.0, 30.0), 1),
            (ScreenPoint::new(30.0, -30.0), 2),
            (ScreenPoint::new(-30.0, -30.0), 3),
            (ScreenPoint::new(-30.0, 30.0), 0),
            (ScreenPoint::new(30.0, 30.0001), 0),
        ];
        for (cursor, slot) in cases {
            assert_eq!(
                geometry.hit(center, center, cursor),
                Hit::Slot(slot),
                "{cursor:?}"
            );
        }
    }

    #[test]
    fn radius_boundaries() {
        let geometry =
            MenuGeometry::new(4, 30.0, 100.0, 1.0, 0.0).unwrap_or_else(|err| panic!("{err}"));
        let center = ScreenPoint::new(0.0, 0.0);
        let cases = [
            (29.999, Hit::Neutral),
            (30.0, Hit::Slot(0)),
            (100.0, Hit::Slot(0)),
            (100.001, Hit::Outside),
        ];
        for (distance, expected) in cases {
            let cursor = ScreenPoint::new(0.0, distance);
            assert_eq!(geometry.hit(center, center, cursor), expected, "{distance}");
        }
    }

    #[test]
    fn non_finite_points_are_outside() {
        let geometry = standard();
        let point = ScreenPoint::new(10.0, 10.0);
        for bad in [
            ScreenPoint::new(f64::NAN, 0.0),
            ScreenPoint::new(0.0, f64::INFINITY),
            ScreenPoint::new(f64::NEG_INFINITY, f64::NAN),
        ] {
            assert_eq!(geometry.hit(bad, point, point), Hit::Outside);
            assert_eq!(geometry.hit(point, bad, point), Hit::Outside);
            assert_eq!(geometry.hit(point, point, bad), Hit::Outside);
        }
    }

    #[test]
    fn geometry_errors() {
        let cases = [
            (
                MenuGeometry::new(1, 10.0, 100.0, 5.0, 0.0),
                GeometryError::TooFewSlots,
            ),
            (
                MenuGeometry::new(4, 0.0, 100.0, 5.0, 0.0),
                GeometryError::InvalidSize,
            ),
            (
                MenuGeometry::new(4, 10.0, -1.0, 5.0, 0.0),
                GeometryError::InvalidSize,
            ),
            (
                MenuGeometry::new(4, 10.0, 100.0, 0.0, 0.0),
                GeometryError::InvalidSize,
            ),
            (
                MenuGeometry::new(4, f64::NAN, 100.0, 5.0, 0.0),
                GeometryError::InvalidSize,
            ),
            (
                MenuGeometry::new(4, 10.0, f64::INFINITY, 5.0, 0.0),
                GeometryError::InvalidSize,
            ),
            (
                MenuGeometry::new(4, 10.0, 100.0, 5.0, f64::NAN),
                GeometryError::InvalidSize,
            ),
            (
                MenuGeometry::new(4, 100.0, 100.0, 5.0, 0.0),
                GeometryError::NeutralNotInside,
            ),
            (
                MenuGeometry::new(4, 120.0, 100.0, 5.0, 0.0),
                GeometryError::NeutralNotInside,
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(result, Err(expected));
        }
    }

    #[test]
    fn placement_keeps_the_menu_on_the_display() {
        let radius = 110.0;
        let cases = [
            // Middle: unchanged.
            ((720.0, 400.0), (720.0, 400.0)),
            // Edges.
            ((3.0, 400.0), (110.0, 400.0)),
            ((1439.0, 400.0), (1330.0, 400.0)),
            ((720.0, 2.0), (720.0, 110.0)),
            ((720.0, 874.0), (720.0, 765.0)),
            // Corners.
            ((0.0, 0.0), (110.0, 110.0)),
            ((0.0, 874.0), (110.0, 765.0)),
            ((1439.0, 0.0), (1330.0, 110.0)),
            ((1439.0, 874.0), (1330.0, 765.0)),
        ];
        for ((x, y), (cx, cy)) in cases {
            assert_eq!(
                place_menu(ScreenPoint::new(x, y), &SCREENS, radius),
                Some(ScreenPoint::new(cx, cy)),
                "invoke ({x}, {y})"
            );
        }
    }

    #[test]
    fn placement_uses_the_display_under_the_point() {
        let radius = 110.0;
        // Left display, negative coordinates, near its bottom-left corner.
        assert_eq!(
            place_menu(ScreenPoint::new(-1915.0, -290.0), &SCREENS, radius),
            Some(ScreenPoint::new(-1810.0, -190.0))
        );
        // Left display near its right edge, next to the primary one.
        assert_eq!(
            place_menu(ScreenPoint::new(-1.0, 500.0), &SCREENS, radius),
            Some(ScreenPoint::new(-110.0, 500.0))
        );
        // Display above, near its top.
        assert_eq!(
            place_menu(ScreenPoint::new(700.0, 1699.0), &SCREENS, radius),
            Some(ScreenPoint::new(700.0, 1590.0))
        );
        // On the shared edge x = 0: the primary display, whose range starts
        // there.
        assert_eq!(
            place_menu(ScreenPoint::new(0.0, 500.0), &SCREENS, radius),
            Some(ScreenPoint::new(110.0, 500.0))
        );
    }

    #[test]
    fn placement_outside_every_display_uses_the_nearest() {
        let radius = 110.0;
        // In the gap between the primary display and the one above.
        assert_eq!(
            place_menu(ScreenPoint::new(700.0, 880.0), &SCREENS, radius),
            Some(ScreenPoint::new(700.0, 765.0))
        );
        // Far to the right of everything.
        assert_eq!(
            place_menu(ScreenPoint::new(5000.0, 100.0), &SCREENS, radius),
            Some(ScreenPoint::new(1330.0, 110.0))
        );
    }

    #[test]
    fn placement_on_a_display_smaller_than_the_menu() {
        let tiny = [ScreenRect::new(0.0, 0.0, 150.0, 1000.0)];
        assert_eq!(
            place_menu(ScreenPoint::new(10.0, 10.0), &tiny, 110.0),
            Some(ScreenPoint::new(75.0, 110.0))
        );
    }

    #[test]
    fn placement_without_displays_or_with_a_bad_point() {
        assert_eq!(place_menu(ScreenPoint::new(1.0, 1.0), &[], 110.0), None);
        assert_eq!(
            place_menu(ScreenPoint::new(f64::NAN, 1.0), &SCREENS, 110.0),
            None
        );
    }

    #[test]
    fn relative_to_and_angles() {
        let center = ScreenPoint::new(10.0, 20.0);
        let offset = ScreenPoint::new(13.0, 24.0).relative_to(center);
        assert_eq!(offset, MenuPoint { x: 3.0, y: 4.0 });
        assert!((offset.length() - 5.0).abs() < 1e-12);
        let up = MenuPoint { x: 0.0, y: 1.0 };
        let right = MenuPoint { x: 1.0, y: 0.0 };
        let down = MenuPoint { x: 0.0, y: -1.0 };
        let left = MenuPoint { x: -1.0, y: 0.0 };
        assert!(up.angle().abs() < 1e-12);
        assert!((right.angle() - PI / 2.0).abs() < 1e-12);
        assert!((down.angle() - PI).abs() < 1e-12);
        assert!((left.angle() - 3.0 * PI / 2.0).abs() < 1e-12);
    }
}
