//! The window of a panel next to the pointer: the toast and the info panel.
//!
//! Like the menu it is a non-activating panel that never becomes key and
//! lets the pointer through, so Finder stays the active application and a
//! click goes to what is below.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSPanel, NSPopUpMenuWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_graphics::CGColor;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use orbit_domain::geometry::ScreenRect;

use super::backdrop::{Backdrop, Background, surface_theme};
use super::palette::{Theme, opaque_fill};

/// Space kept between a panel and the edges of the visible frame, in points.
pub const SCREEN_MARGIN: f64 = 8.0;

/// A hidden panel window with a background.
pub struct Overlay {
    window: Retained<NSPanel>,
    backdrop: Backdrop,
    background: Background,
    corner_radius: f64,
}

impl Overlay {
    /// A hidden window with `background` and rounded corners.
    pub fn new(mtm: MainThreadMarker, background: Background, corner_radius: f64) -> Self {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: the panel is owned by `Overlay`, not released on close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(true);
        window.setLevel(NSPopUpMenuWindowLevel);
        // A click must go to the application below.
        window.setIgnoresMouseEvents(true);
        window.setHidesOnDeactivate(false);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Transient,
        );
        let content = NSView::initWithFrame(NSView::alloc(mtm), frame);
        window.setContentView(Some(&content));
        let backdrop = Backdrop::new(mtm, background, frame, corner_radius);
        content.addSubview(&backdrop.view);
        Self {
            window,
            backdrop,
            background,
            corner_radius,
        }
    }

    /// The view that holds the content; its coordinates start at the bottom
    /// left corner of the panel.
    pub fn content(&self) -> &NSView {
        &self.backdrop.content
    }

    /// Puts the window at `frame` and shows it. The content must already be
    /// laid out for the size of `frame`. Returns the theme of the
    /// background, which the content is drawn for.
    pub fn show(&self, mtm: MainThreadMarker, frame: ScreenRect) -> Theme {
        let reduce_transparency =
            NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceTransparency();
        let size = NSSize::new(frame.width, frame.height);
        let local = NSRect::new(NSPoint::new(0.0, 0.0), size);
        self.window.setFrame_display(
            NSRect::new(NSPoint::new(frame.origin.x, frame.origin.y), size),
            false,
        );
        if let Some(content) = self.window.contentView() {
            content.setFrame(local);
        }
        self.backdrop.view.setFrame(local);
        self.backdrop.content.setFrame(local);
        let theme = surface_theme(
            mtm,
            self.background,
            &self.backdrop.view,
            reduce_transparency,
        );
        let opaque = reduce_transparency || self.background == Background::Plain;
        if let Some(layer) = self.backdrop.content.layer() {
            if opaque {
                let fill = opaque_fill(theme);
                let color = CGColor::new_srgb(fill.r, fill.g, fill.b, fill.a);
                layer.setBackgroundColor(Some(&color));
                layer.setCornerRadius(self.corner_radius);
            } else {
                layer.setBackgroundColor(None);
            }
        }
        self.window.orderFrontRegardless();
        theme
    }

    /// Hides the window. Does nothing when it is hidden.
    pub fn hide(&self) {
        self.window.orderOut(None);
    }
}
