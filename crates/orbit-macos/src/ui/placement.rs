//! Where a panel next to the pointer goes: the toast and the info panel.
//!
//! The display is chosen by its whole frame, because the menu bar and the
//! Dock are on the display but outside its visible frame. The panel then
//! goes inside the visible frame of that display, so it never covers the
//! menu bar or the Dock, and is made smaller when it does not fit.

use orbit_domain::geometry::{ScreenPoint, ScreenRect};

use crate::platform::coordinates::Display;

/// Distance between the pointer and the panel, in points.
pub const GAP: f64 = 12.0;

/// The frame of a panel of `wanted` size (width, height) next to `anchor`,
/// in Cocoa coordinates, or `None` when there is no display.
///
/// 1. The display is the one whose frame holds `anchor`; when none does, the
///    nearest one, and on a tie the first (the primary).
/// 2. The area is its visible frame without `margin` on each side. The panel
///    is never larger than the area.
/// 3. The panel goes right of and below the pointer, [`GAP`] away; on the
///    left or above when it does not fit there; then it is moved inside the
///    area.
pub fn place_panel(
    anchor: ScreenPoint,
    wanted: (f64, f64),
    screens: &[Display],
    margin: f64,
) -> Option<ScreenRect> {
    let area = panel_area(anchor, screens, margin)?;
    let width = wanted.0.min(area.width).max(0.0);
    let height = wanted.1.min(area.height).max(0.0);
    let right = area.origin.x + area.width;
    let top = area.origin.y + area.height;

    let mut x = anchor.x + GAP;
    if x + width > right {
        x = anchor.x - GAP - width;
    }
    let mut y = anchor.y - GAP - height;
    if y < area.origin.y {
        y = anchor.y + GAP;
    }
    let x = x.clamp(area.origin.x, right - width);
    let y = y.clamp(area.origin.y, top - height);
    Some(ScreenRect::new(x, y, width, height))
}

/// The area a panel next to `anchor` stays inside: the visible frame of the
/// display chosen as in [`place_panel`], without `margin` on each side; or
/// `None` when there is no display. A panel that depends on its width for
/// its height learns that width from here before it is laid out.
pub fn panel_area(anchor: ScreenPoint, screens: &[Display], margin: f64) -> Option<ScreenRect> {
    display_at(anchor, screens).map(|display| inset(display.visible, margin))
}

/// The display that holds `point`, or the nearest one.
fn display_at(point: ScreenPoint, screens: &[Display]) -> Option<&Display> {
    if let Some(display) = screens.iter().find(|d| d.frame.contains(point)) {
        return Some(display);
    }
    let mut nearest: Option<(&Display, f64)> = None;
    for display in screens {
        let distance = display.frame.distance_to(point);
        // Strictly closer only: on a tie the earlier display stays.
        if nearest.is_none_or(|(_, best)| distance < best) {
            nearest = Some((display, distance));
        }
    }
    nearest.map(|(display, _)| display)
}

/// `rect` without `margin` on each side; never negative.
fn inset(rect: ScreenRect, margin: f64) -> ScreenRect {
    let width = (rect.width - 2.0 * margin).max(0.0);
    let height = (rect.height - 2.0 * margin).max(0.0);
    ScreenRect::new(
        rect.origin.x + (rect.width - width) / 2.0,
        rect.origin.y + (rect.height - height) / 2.0,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARGIN: f64 = 8.0;
    const SIZE: (f64, f64) = (300.0, 200.0);

    /// The primary display: 1440 × 900, menu bar 25 pt, Dock 70 pt at the
    /// bottom.
    fn primary() -> Display {
        Display {
            frame: ScreenRect::new(0.0, 0.0, 1440.0, 900.0),
            visible: ScreenRect::new(0.0, 70.0, 1440.0, 805.0),
        }
    }

    /// A second display right of the primary, with its own menu bar.
    fn right() -> Display {
        Display {
            frame: ScreenRect::new(1440.0, 0.0, 1920.0, 1080.0),
            visible: ScreenRect::new(1440.0, 0.0, 1920.0, 1055.0),
        }
    }

    /// A display left of and below the primary: negative origin.
    fn left_below() -> Display {
        Display {
            frame: ScreenRect::new(-1280.0, -300.0, 1280.0, 1024.0),
            visible: ScreenRect::new(-1280.0, -300.0, 1280.0, 999.0),
        }
    }

    fn place(x: f64, y: f64, screens: &[Display]) -> ScreenRect {
        place_panel(ScreenPoint::new(x, y), SIZE, screens, MARGIN)
            .unwrap_or_else(|| panic!("no frame for ({x}, {y})"))
    }

    /// The whole panel is inside the area of `display`.
    fn inside(frame: ScreenRect, display: Display) {
        let area = inset(display.visible, MARGIN);
        assert!(
            frame.origin.x >= area.origin.x,
            "{frame:?} left of {area:?}"
        );
        assert!(frame.origin.y >= area.origin.y, "{frame:?} below {area:?}");
        assert!(
            frame.origin.x + frame.width <= area.origin.x + area.width,
            "{frame:?} right of {area:?}"
        );
        assert!(
            frame.origin.y + frame.height <= area.origin.y + area.height,
            "{frame:?} above {area:?}"
        );
    }

    #[test]
    fn in_the_middle_the_panel_is_right_of_and_below_the_pointer() {
        let frame = place(700.0, 500.0, &[primary()]);
        assert_eq!(frame, ScreenRect::new(712.0, 288.0, 300.0, 200.0));
    }

    #[test]
    fn at_the_right_edge_the_panel_goes_left_of_the_pointer() {
        let frame = place(1430.0, 500.0, &[primary()]);
        assert_eq!(frame.origin.x, 1430.0 - GAP - 300.0);
        inside(frame, primary());
    }

    #[test]
    fn above_the_dock_the_panel_goes_above_the_pointer() {
        let frame = place(700.0, 100.0, &[primary()]);
        assert_eq!(frame.origin.y, 100.0 + GAP);
        inside(frame, primary());
    }

    #[test]
    fn over_the_dock_the_panel_stays_out_of_it() {
        // The pointer is on the Dock: on the display, outside the visible
        // frame.
        let frame = place(700.0, 20.0, &[primary()]);
        assert_eq!(frame.origin.y, 70.0 + MARGIN);
        inside(frame, primary());
    }

    #[test]
    fn in_a_corner_the_panel_stays_on_the_display() {
        for (x, y) in [(0.0, 0.0), (1439.0, 0.0), (0.0, 899.0), (1439.0, 899.0)] {
            inside(place(x, y, &[primary()]), primary());
        }
    }

    #[test]
    fn in_the_menu_bar_of_the_second_display_the_panel_stays_below_it() {
        let screens = [primary(), right()];
        let frame = place(2000.0, 1070.0, &screens);
        inside(frame, right());
    }

    #[test]
    fn on_the_dock_of_the_second_display_the_panel_stays_there() {
        // The Dock is on the left of the second display, next to the primary.
        let docked = Display {
            frame: ScreenRect::new(1440.0, 0.0, 1920.0, 1080.0),
            visible: ScreenRect::new(1510.0, 0.0, 1850.0, 1055.0),
        };
        // On that Dock the visible frame of the primary (10 pt away) is
        // nearer than that of the second display (60 pt): a choice by
        // visible frames would put the panel on the wrong display.
        let frame = place(1450.0, 500.0, &[primary(), docked]);
        inside(frame, docked);
    }

    #[test]
    fn at_the_shared_edge_the_display_right_of_it_is_chosen() {
        let screens = [primary(), right()];
        // The edge belongs to the display that starts there.
        inside(place(1440.0, 500.0, &screens), right());
        inside(place(1439.9, 500.0, &screens), primary());
    }

    #[test]
    fn a_display_with_a_negative_origin_is_used() {
        let screens = [primary(), left_below()];
        let frame = place(-1270.0, -290.0, &screens);
        inside(frame, left_below());
    }

    #[test]
    fn a_pointer_off_every_display_uses_the_nearest() {
        let screens = [primary(), right()];
        inside(place(4000.0, 500.0, &screens), right());
        inside(place(-50.0, 500.0, &screens), primary());
    }

    #[test]
    fn on_a_tie_the_primary_is_used() {
        // Halfway between two displays with a gap between them.
        let a = Display {
            frame: ScreenRect::new(0.0, 0.0, 100.0, 100.0),
            visible: ScreenRect::new(0.0, 0.0, 100.0, 100.0),
        };
        let b = Display {
            frame: ScreenRect::new(200.0, 0.0, 100.0, 100.0),
            visible: ScreenRect::new(200.0, 0.0, 100.0, 100.0),
        };
        let at = ScreenPoint::new(150.0, 50.0);
        assert_eq!(display_at(at, &[a, b]), Some(&a));
        assert_eq!(display_at(at, &[b, a]), Some(&b));
    }

    #[test]
    fn a_panel_taller_than_the_display_is_cut_to_it() {
        let frame = place_panel(
            ScreenPoint::new(700.0, 500.0),
            (300.0, 5000.0),
            &[primary()],
            MARGIN,
        )
        .unwrap_or_else(|| panic!("no frame"));
        assert_eq!(frame.height, 805.0 - 2.0 * MARGIN);
        inside(frame, primary());
    }

    #[test]
    fn a_panel_wider_than_the_display_is_cut_to_it() {
        let frame = place_panel(
            ScreenPoint::new(700.0, 500.0),
            (9000.0, 200.0),
            &[primary()],
            MARGIN,
        )
        .unwrap_or_else(|| panic!("no frame"));
        assert_eq!(frame.width, 1440.0 - 2.0 * MARGIN);
        inside(frame, primary());
    }

    #[test]
    fn the_area_is_the_one_the_panel_is_kept_inside() {
        let screens = [primary(), left_below()];
        // On a display with a negative origin, and outside every display.
        for (x, y) in [(-600.0, 100.0), (-5000.0, -5000.0)] {
            let anchor = ScreenPoint::new(x, y);
            let area = panel_area(anchor, &screens, MARGIN);
            assert_eq!(area, Some(inset(left_below().visible, MARGIN)));
            inside(place(x, y, &screens), left_below());
        }
        assert_eq!(panel_area(ScreenPoint::new(0.0, 0.0), &[], MARGIN), None);
    }

    #[test]
    fn no_display_gives_no_frame() {
        assert_eq!(
            place_panel(ScreenPoint::new(0.0, 0.0), SIZE, &[], MARGIN),
            None
        );
    }
}
