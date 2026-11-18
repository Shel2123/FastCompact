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
use objc2_foundation::NSRect;
use orbit_domain::geometry::{ScreenPoint, ScreenRect};

use super::event_tap::tracker::QuartzPoint;

/// The displays as the controller and the panels need them.
#[derive(Debug, Clone, PartialEq)]
pub struct Displays {
    /// Height of the primary display, which ties the two coordinate systems.
    pub primary_height: f64,
    /// All displays, the primary first.
    pub screens: Vec<Display>,
}

/// One display in Cocoa coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Display {
    /// The whole display. Tells which display a point is on: the menu bar and
    /// the Dock are outside `visible` but still on the display.
    pub frame: ScreenRect,
    /// The display without the menu bar and the Dock; where windows go.
    pub visible: ScreenRect,
}

impl Displays {
    /// The displays now, or `None` if there are none.
    pub fn current(mtm: MainThreadMarker) -> Option<Self> {
        let screens = NSScreen::screens(mtm);
        let primary = screens.iter().next()?.frame();
        let screens = screens
            .iter()
            .map(|screen| Display {
                frame: rect(screen.frame()),
                visible: rect(screen.visibleFrame()),
            })
            .collect();
        Some(Self {
            primary_height: primary.size.height,
            screens,
        })
    }

    /// Visible frames of all displays, for placing the menu.
    pub fn visible(&self) -> Vec<ScreenRect> {
        self.screens.iter().map(|screen| screen.visible).collect()
    }

    /// `point` in Cocoa coordinates.
    pub fn to_cocoa(&self, point: QuartzPoint) -> ScreenPoint {
        ScreenPoint::new(point.x, self.primary_height - point.y)
    }
}

fn rect(frame: NSRect) -> ScreenRect {
    ScreenRect::new(
        frame.origin.x,
        frame.origin.y,
        frame.size.width,
        frame.size.height,
    )
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
            screens: vec![
                // Primary, with the menu bar.
                Display {
                    frame: ScreenRect::new(0.0, 0.0, 1440.0, 900.0),
                    visible: ScreenRect::new(0.0, 0.0, 1440.0, 875.0),
                },
                // Above the primary.
                Display {
                    frame: ScreenRect::new(-200.0, 900.0, 1920.0, 1080.0),
                    visible: ScreenRect::new(-200.0, 900.0, 1920.0, 1080.0),
                },
                // Left of the primary, lower.
                Display {
                    frame: ScreenRect::new(-1280.0, -300.0, 1280.0, 1024.0),
                    visible: ScreenRect::new(-1280.0, -300.0, 1280.0, 1024.0),
                },
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
        assert!(inside(d.screens[1].visible, above));
        // Left of the primary and lower: negative x, Quartz y past its
        // bottom.
        let left = d.to_cocoa(QuartzPoint::new(-640.0, 1000.0));
        assert_eq!(left, ScreenPoint::new(-640.0, -100.0));
        assert!(inside(d.screens[2].visible, left));
    }
}
