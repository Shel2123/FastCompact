//! Conversion from Quartz to Cocoa screen coordinates.
//!
//! The tap reports positions in Quartz coordinates: origin at the top left
//! of the primary display, y growing down. The controller works in Cocoa
//! coordinates: origin at the bottom left of the primary display, y growing
//! up. Both share the primary display, the first of `NSScreen.screens`,
//! which holds the menu bar and has its origin at (0, 0). `NSScreen.main` is
//! the display with keyboard focus and can be another one.

use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use orbit_domain::geometry::{ScreenPoint, ScreenRect};

use super::event_tap::tracker::QuartzPoint;

/// The displays as the controller needs them.
#[derive(Debug, Clone, PartialEq)]
pub struct Displays {
    /// Height of the primary display, which ties the two coordinate systems.
    pub primary_height: f64,
    /// Visible frames of all displays, without menu bar and Dock, for
    /// placing the menu.
    pub visible: Vec<ScreenRect>,
}

impl Displays {
    /// The displays now, or `None` if there are none.
    pub fn current(mtm: MainThreadMarker) -> Option<Self> {
        let screens = NSScreen::screens(mtm);
        let primary = screens.iter().next()?.frame();
        let visible = screens
            .iter()
            .map(|screen| {
                let frame = screen.visibleFrame();
                ScreenRect::new(
                    frame.origin.x,
                    frame.origin.y,
                    frame.size.width,
                    frame.size.height,
                )
            })
            .collect();
        Some(Self {
            primary_height: primary.size.height,
            visible,
        })
    }

    /// `point` in Cocoa coordinates.
    pub fn to_cocoa(&self, point: QuartzPoint) -> ScreenPoint {
        ScreenPoint::new(point.x, self.primary_height - point.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inside(rect: ScreenRect, point: ScreenPoint) -> bool {
        (rect.origin.x..rect.origin.x + rect.width).contains(&point.x)
            && (rect.origin.y..rect.origin.y + rect.height).contains(&point.y)
    }

    fn displays() -> Displays {
        Displays {
            primary_height: 900.0,
            visible: vec![
                // Primary, with the menu bar.
                ScreenRect::new(0.0, 0.0, 1440.0, 875.0),
                // Above the primary.
                ScreenRect::new(-200.0, 900.0, 1920.0, 1080.0),
                // Left of the primary, lower.
                ScreenRect::new(-1280.0, -300.0, 1280.0, 1024.0),
            ],
        }
    }

    #[test]
    fn points_on_the_primary_display_flip_around_its_height() {
        let d = displays();
        assert_eq!(
            d.to_cocoa(QuartzPoint::new(0.0, 0.0)),
            ScreenPoint::new(0.0, 900.0)
        );
        assert_eq!(
            d.to_cocoa(QuartzPoint::new(700.0, 500.0)),
            ScreenPoint::new(700.0, 400.0)
        );
        assert_eq!(
            d.to_cocoa(QuartzPoint::new(1439.0, 900.0)),
            ScreenPoint::new(1439.0, 0.0)
        );
    }

    #[test]
    fn points_on_other_displays_keep_their_offsets() {
        let d = displays();
        // Above the primary: negative Quartz y.
        let above = d.to_cocoa(QuartzPoint::new(100.0, -500.0));
        assert_eq!(above, ScreenPoint::new(100.0, 1400.0));
        assert!(inside(d.visible[1], above));
        // Left of the primary and lower: negative x, Quartz y past its
        // bottom.
        let left = d.to_cocoa(QuartzPoint::new(-640.0, 1000.0));
        assert_eq!(left, ScreenPoint::new(-640.0, -100.0));
        assert!(inside(d.visible[2], left));
    }
}
