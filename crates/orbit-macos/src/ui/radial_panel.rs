//! The radial menu window: a non-activating panel over everything, with the
//! glass disc and the [`Surface`](super::radial_view::Surface) in it.
//!
//! [`RadialMenu`] is the [`MenuView`] of the macOS platform. It draws what
//! the controller says and never reads input: the pointer comes from the
//! event tap, the controller hit-tests, and the view only paints. There are
//! no timers, observers or event monitors, so an idle menu costs nothing.
//!
//! Everything is called on the main thread (`MainThreadMarker`), one call at
//! a time, as the contract of [`MenuView`] says.

use std::time::Instant;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSColorSpace, NSPanel, NSPopUpMenuWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::CATransaction;
use orbit_app::catalog::{MenuModel, SLOT_COUNT};
use orbit_app::ports::MenuView;
use orbit_app::state::Highlight;
use orbit_domain::geometry::MenuGeometry;
use thiserror::Error;
use tracing::{debug, error, warn};

use super::backdrop::{Backdrop, Background, app_theme, surface_theme};
use super::menu_session::{HighlightChange, Session};
use super::palette::{Palette, Rgba, Theme};
use super::radial_view::{MARGIN, Surface, disc_side, drawable, place_disc, window_side};

/// How long a highlight takes to change, in seconds.
const HIGHLIGHT_SECONDS: f64 = 0.07;
/// A window that landed closer than this to where it was put is taken to be
/// there, in points. The window is asked for whole points and gets them, so
/// any real move is a whole point; this only absorbs readback noise.
const SHIFT_TOLERANCE: f64 = 0.5;

/// Why the menu window could not be built.
#[derive(Debug, Error)]
pub enum ViewError {
    /// The geometry has no sector for some item of the menu.
    #[error("the menu cannot be drawn with this geometry")]
    NotDrawable,
    /// A view that should host the layers has no layer.
    #[error("the view that hosts the menu layers has no layer")]
    NoLayer,
}

/// The window, the disc inside it and the layers.
struct Panel {
    geometry: MenuGeometry,
    window: Retained<NSPanel>,
    /// The background view; the layers live inside it and it is the one thing
    /// that moves when the window lands off the requested place.
    disc: Retained<NSView>,
    surface: Surface,
}

impl Panel {
    fn build(
        mtm: MainThreadMarker,
        geometry: &MenuGeometry,
        background: Background,
    ) -> Result<Self, ViewError> {
        if !drawable(geometry) {
            return Err(ViewError::NotDrawable);
        }
        let side = window_side(geometry);
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(side, side));
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: the panel is owned by `Panel`, not released on close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        window.setLevel(NSPopUpMenuWindowLevel);
        // The pointer never reaches the menu: the controller gets it from the
        // event tap, and a click must go to the application below.
        window.setIgnoresMouseEvents(true);
        window.setHidesOnDeactivate(false);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Transient,
        );
        let content = NSView::initWithFrame(NSView::alloc(mtm), frame);
        window.setContentView(Some(&content));

        let scale = window.backingScaleFactor();
        let palette = system_palette(app_theme(mtm));
        let surface = Surface::build(geometry, scale, palette).ok_or(ViewError::NotDrawable)?;
        let Backdrop {
            view: disc,
            content: host,
        } = background_view(mtm, background, geometry);
        let host = host.layer().ok_or(ViewError::NoLayer)?;
        host.addSublayer(surface.root());
        content.addSubview(&disc);
        Ok(Self {
            geometry: *geometry,
            window,
            disc,
            surface,
        })
    }
}

/// The background view of the disc and the view that hosts the drawing.
/// The drawing lives inside the background view, so that the glass and the
/// sectors are one thing.
fn background_view(
    mtm: MainThreadMarker,
    background: Background,
    geometry: &MenuGeometry,
) -> Backdrop {
    let side = disc_side(geometry);
    let frame = NSRect::new(NSPoint::new(MARGIN, MARGIN), NSSize::new(side, side));
    Backdrop::new(mtm, background, frame, geometry.outer_radius())
}

/// The radial menu on screen.
pub struct RadialMenu {
    mtm: MainThreadMarker,
    background: Background,
    /// `ORBIT_DEBUG_MENU_SHIFT`: puts the window this far from where it was
    /// asked to be, to see the disc follow it.
    debug_shift: (i32, i32),
    panel: Panel,
    session: Session,
}

impl RadialMenu {
    /// Builds the window and the layers, hidden.
    ///
    /// `background` is the one asked for (a debug switch); `None` or glass on
    /// a system without `NSGlassEffectView` gets the best the system has.
    /// `debug_shift` is `(0, 0)` outside the debug switch.
    ///
    /// # Errors
    ///
    /// [`ViewError`] when the layers cannot be built or hosted.
    pub fn new(
        mtm: MainThreadMarker,
        background: Option<Background>,
        debug_shift: (i32, i32),
    ) -> Result<Self, ViewError> {
        let background = Background::available(background);
        let panel = Panel::build(mtm, &MenuGeometry::standard(), background)?;
        Ok(Self {
            mtm,
            background,
            debug_shift,
            panel,
            session: Session::new(),
        })
    }

    /// The model cannot be shown: the previous picture must not stay.
    fn refuse(&mut self) {
        self.session.refuse();
        self.panel.window.orderOut(None);
    }

    fn show(&mut self, model: &MenuModel, started: Instant) {
        let workspace = NSWorkspace::sharedWorkspace();
        let reduce_transparency = workspace.accessibilityDisplayShouldReduceTransparency();
        let theme = surface_theme(
            self.mtm,
            self.background,
            &self.panel.disc,
            reduce_transparency,
        );
        let palette = system_palette(theme);
        let available = model.slots.map(|slot| slot.availability.is_available());
        let opaque_disc = reduce_transparency || self.background == Background::Plain;

        let panel = &mut self.panel;
        let side = window_side(&model.geometry);
        let disc = disc_side(&model.geometry);
        let exact = (model.center.x - side / 2.0, model.center.y - side / 2.0);
        // AppKit keeps a window on whole points and drops the fraction of the
        // pointer, not always to the nearest point. The window is asked for
        // whole points, so that it lands where asked, and the disc takes the
        // fraction inside it.
        let asked = (exact.0.round(), exact.1.round());
        let (dx, dy) = self.debug_shift;
        panel.window.setFrameOrigin(NSPoint::new(
            asked.0 + f64::from(dx),
            asked.1 + f64::from(dy),
        ));
        // Where AppKit put the window is what counts: the disc must sit at
        // the center of the model, not at the center of the window.
        let actual = panel.window.frame().origin;
        let offset = (exact.0 - actual.x, exact.1 - actual.y);
        let scale = panel.window.backingScaleFactor();
        let placement = place_disc(side, disc, offset, scale);
        let shift = (snap(asked.0 - actual.x), snap(asked.1 - actual.y));
        if shift != (0.0, 0.0) {
            warn!(?shift, ?placement, "the window is not where it was put");
        }

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        panel.surface.reset(palette, available, scale, opaque_disc);
        panel
            .disc
            .setFrameOrigin(NSPoint::new(placement.origin.0, placement.origin.1));
        CATransaction::commit();
        panel.window.orderFrontRegardless();

        // A window that is not on screen may not know its display yet.
        let shown_scale = panel.window.backingScaleFactor();
        if (shown_scale - scale).abs() >= f64::EPSILON {
            debug!(
                scale,
                shown_scale, "the display scale differs after the show"
            );
            // The pixel grid changed with the display.
            let placement = place_disc(side, disc, offset, shown_scale);
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            panel
                .surface
                .reset(palette, available, shown_scale, opaque_disc);
            panel
                .disc
                .setFrameOrigin(NSPoint::new(placement.origin.0, placement.origin.1));
            CATransaction::commit();
        }
        if cfg!(debug_assertions) {
            let animated = panel.surface.animated_layers();
            if animated > 0 {
                warn!(animated, "layers have animations after the show");
            }
        }
        debug!(elapsed = ?started.elapsed(), scale = shown_scale, ?theme, "menu shown");
    }
}

impl MenuView for RadialMenu {
    fn present(&mut self, model: &MenuModel) {
        let started = Instant::now();
        if !drawable(&model.geometry) {
            error!("the menu geometry has no sector for each item; nothing is shown");
            self.refuse();
            return;
        }
        if self.panel.geometry != model.geometry {
            warn!("the menu geometry changed; building the layers again");
            match Panel::build(self.mtm, &model.geometry, self.background) {
                Ok(panel) => {
                    self.panel.window.orderOut(None);
                    self.panel = panel;
                }
                Err(err) => {
                    error!(%err, "the menu cannot be built; nothing is shown");
                    self.refuse();
                    return;
                }
            }
        }
        self.session.present();
        self.show(model, started);
    }

    fn highlight(&mut self, highlight: Highlight) {
        match self.session.highlight(highlight, SLOT_COUNT) {
            HighlightChange::Closed => {
                debug!(?highlight, "highlight while no menu is open ignored");
            }
            HighlightChange::Unchanged => {}
            HighlightChange::OutOfRange => {
                warn!(?highlight, "highlight of a slot outside the menu ignored");
            }
            HighlightChange::Changed { from, to } => {
                self.panel.surface.highlight(from, to, HIGHLIGHT_SECONDS);
            }
        }
    }

    fn hide(&mut self) {
        if self.session.hide() {
            self.panel.window.orderOut(None);
        }
    }
}

/// A difference that is rounding noise counts as none.
fn snap(difference: f64) -> f64 {
    if difference.abs() < SHIFT_TOLERANCE {
        0.0
    } else {
        difference
    }
}

/// The palette for `theme` with the accent and the red the user has now.
/// Dynamic system colors are read here, on every show, so a changed accent
/// shows at the next menu. They are resolved for the appearance of the
/// application, which can differ from that of the glass view.
fn system_palette(theme: Theme) -> Palette {
    const FALLBACK_ACCENT: Rgba = Rgba::new(0.0, 0.478, 1.0, 1.0);
    const FALLBACK_RED: Rgba = Rgba::new(1.0, 0.231, 0.188, 1.0);
    let accent = srgb(&NSColor::controlAccentColor()).unwrap_or_else(|| {
        warn!("the accent color has no sRGB form; using blue");
        FALLBACK_ACCENT
    });
    let red = srgb(&NSColor::systemRedColor()).unwrap_or_else(|| {
        warn!("the system red has no sRGB form; using a fixed red");
        FALLBACK_RED
    });
    Palette::new(theme, accent, red)
}

fn srgb(color: &NSColor) -> Option<Rgba> {
    let rgb = color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    Some(Rgba::new(
        rgb.redComponent(),
        rgb.greenComponent(),
        rgb.blueComponent(),
        1.0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_noise_is_not_a_shift() {
        assert_eq!(snap(0.2), 0.0);
        assert_eq!(snap(-0.49), 0.0);
        assert_eq!(snap(0.5), 0.5);
        assert_eq!(snap(-6.0), -6.0);
    }
}
