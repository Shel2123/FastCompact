//! Minimal radial panel: a borderless non-activating `NSPanel` that never
//! becomes key and ignores mouse events, so Finder keeps focus and the global
//! mouse monitor sees every move.

use std::cell::Cell;
use std::f64::consts::PI;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSColor, NSPanel, NSPopUpMenuWindowLevel, NSScreen, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

pub const SECTORS: usize = 8;
/// The X sector keeps a fixed place: straight down.
pub const CANCEL_SECTOR: usize = 4;

const SIZE: f64 = 240.0;
const INNER_RADIUS: f64 = 30.0;
const OUTER_RADIUS: f64 = 110.0;

pub struct RadialPanel {
    window: Retained<NSPanel>,
    view: Retained<RadialView>,
}

impl RadialPanel {
    pub fn new(mtm: MainThreadMarker) -> Self {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIZE, SIZE));
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: the panel is owned by `RadialPanel`, not released on close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        window.setLevel(NSPopUpMenuWindowLevel);
        window.setIgnoresMouseEvents(true);
        window.setHidesOnDeactivate(false);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Transient,
        );
        let view = RadialView::new(mtm, frame);
        window.setContentView(Some(&view));
        Self { window, view }
    }

    /// Shows the panel centred on `point` (Cocoa coordinates), shifted to fit
    /// the visible frame of its screen. Returns the actual centre.
    pub fn show(&self, mtm: MainThreadMarker, point: NSPoint) -> NSPoint {
        let mut origin = NSPoint::new(point.x - SIZE / 2.0, point.y - SIZE / 2.0);
        if let Some(visible) = visible_frame_containing(mtm, point) {
            let max_x = visible.origin.x + visible.size.width - SIZE;
            let max_y = visible.origin.y + visible.size.height - SIZE;
            origin.x = origin.x.min(max_x).max(visible.origin.x);
            origin.y = origin.y.min(max_y).max(visible.origin.y);
        }
        self.window.setFrameOrigin(origin);
        self.view.set_selected(None);
        self.window.orderFrontRegardless();
        NSPoint::new(origin.x + SIZE / 2.0, origin.y + SIZE / 2.0)
    }

    pub fn hide(&self) {
        self.window.orderOut(None);
    }

    /// Updates the highlight for the cursor (Cocoa coordinates).
    pub fn track(&self, cursor: NSPoint) {
        let frame = self.window.frame();
        let dx = cursor.x - (frame.origin.x + SIZE / 2.0);
        let dy = cursor.y - (frame.origin.y + SIZE / 2.0);
        self.view.set_selected(sector_at(dx, dy));
    }

    pub fn selected(&self) -> Option<usize> {
        self.view.ivars().selected.get()
    }
}

/// Sector under an offset from the centre (y up), clockwise from the top.
/// `None` in the neutral centre and outside the ring.
fn sector_at(dx: f64, dy: f64) -> Option<usize> {
    let distance = dx.hypot(dy);
    if !(INNER_RADIUS..=OUTER_RADIUS).contains(&distance) {
        return None;
    }
    let clockwise_from_top = (90.0 - dy.atan2(dx).to_degrees()).rem_euclid(360.0);
    let width = 360.0 / SECTORS as f64;
    Some(((clockwise_from_top + width / 2.0) / width) as usize % SECTORS)
}

fn visible_frame_containing(mtm: MainThreadMarker, point: NSPoint) -> Option<NSRect> {
    NSScreen::screens(mtm).iter().find_map(|screen| {
        let frame = screen.frame();
        let inside = point.x >= frame.origin.x
            && point.x < frame.origin.x + frame.size.width
            && point.y >= frame.origin.y
            && point.y < frame.origin.y + frame.size.height;
        inside.then(|| screen.visibleFrame())
    })
}

pub struct ViewIvars {
    selected: Cell<Option<usize>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ViewIvars]
    struct RadialView;

    impl RadialView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }
    }
);

impl RadialView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            selected: Cell::new(None),
        });
        // SAFETY: NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn set_selected(&self, sector: Option<usize>) {
        if self.ivars().selected.replace(sector) != sector {
            self.setNeedsDisplay(true);
        }
    }

    fn draw(&self) {
        let bounds = self.bounds();
        let center = NSPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let width = 360.0 / SECTORS as f64;
        let selected = self.ivars().selected.get();
        for sector in 0..SECTORS {
            // Convert "clockwise from the top" into AppKit angles (CCW from +x).
            let start = 90.0 - (sector as f64 * width - width / 2.0);
            let end = start - width;
            let path = NSBezierPath::bezierPath();
            path.moveToPoint(center);
            path.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
                center,
                OUTER_RADIUS,
                start,
                end,
                true,
            );
            path.closePath();
            let (r, g, b) = match (Some(sector) == selected, sector == CANCEL_SECTOR) {
                (true, true) => (0.85, 0.25, 0.25),
                (true, false) => (0.20, 0.50, 0.95),
                (false, true) => (0.45, 0.20, 0.20),
                (false, false) => (0.20, 0.20, 0.22),
            };
            NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 0.92).setFill();
            path.fill();
            NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.35).setStroke();
            path.setLineWidth(1.0);
            path.stroke();
        }
        let neutral = NSRect::new(
            NSPoint::new(center.x - INNER_RADIUS, center.y - INNER_RADIUS),
            NSSize::new(INNER_RADIUS * 2.0, INNER_RADIUS * 2.0),
        );
        NSColor::colorWithSRGBRed_green_blue_alpha(0.10, 0.10, 0.11, 0.95).setFill();
        NSBezierPath::bezierPathWithOvalInRect(neutral).fill();
        // Small mark on the X sector so its place is visible.
        let mark_angle = (90.0 - CANCEL_SECTOR as f64 * width) * PI / 180.0;
        let mark_radius = (INNER_RADIUS + OUTER_RADIUS) / 2.0;
        let mark = NSPoint::new(
            center.x + mark_radius * mark_angle.cos(),
            center.y + mark_radius * mark_angle.sin(),
        );
        let cross = NSBezierPath::bezierPath();
        cross.moveToPoint(NSPoint::new(mark.x - 8.0, mark.y - 8.0));
        cross.lineToPoint(NSPoint::new(mark.x + 8.0, mark.y + 8.0));
        cross.moveToPoint(NSPoint::new(mark.x - 8.0, mark.y + 8.0));
        cross.lineToPoint(NSPoint::new(mark.x + 8.0, mark.y - 8.0));
        cross.setLineWidth(2.5);
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.9).setStroke();
        cross.stroke();
    }
}
