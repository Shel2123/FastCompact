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
use objc2::runtime::AnyClass;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSApplication, NSBackingStoreType, NSColor, NSColorSpace, NSGlassEffectView, NSPanel,
    NSPopUpMenuWindowLevel, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindowCollectionBehavior, NSWindowStyleMask,
    NSWorkspace,
};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransaction};
use orbit_app::catalog::{MenuModel, SLOT_COUNT};
use orbit_app::ports::MenuView;
use orbit_app::state::Highlight;
use orbit_domain::geometry::MenuGeometry;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use super::menu_session::{HighlightChange, Session};
use super::palette::{Palette, Rgba, Theme};
use super::radial_view::{MARGIN, Surface, disc_side, drawable, place_disc, window_side};

/// How long a highlight takes to change, in seconds.
const HIGHLIGHT_SECONDS: f64 = 0.07;
/// A window that landed closer than this to where it was put is taken to be
/// there, in points: the readback is not exact below a pixel.
const SHIFT_TOLERANCE: f64 = 0.5;

/// What is behind the sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    /// `NSGlassEffectView`, macOS 26 and later.
    Glass,
    /// `NSVisualEffectView` with the HUD material.
    Vibrancy,
    /// A plain translucent disc.
    Plain,
}

impl Background {
    /// The background named `name` (`glass`, `vibrancy`, `plain`).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "glass" => Some(Self::Glass),
            "vibrancy" => Some(Self::Vibrancy),
            "plain" => Some(Self::Plain),
            _ => None,
        }
    }
}

/// The background to use, given the one asked for and whether the system has
/// `NSGlassEffectView`. The second value is `true` when glass was wanted but
/// the system lacks it.
fn resolve_background(requested: Option<Background>, glass_available: bool) -> (Background, bool) {
    match requested {
        None | Some(Background::Glass) if glass_available => (Background::Glass, false),
        None | Some(Background::Glass) => (Background::Vibrancy, true),
        Some(other) => (other, false),
    }
}

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
        let (disc, host) = background_view(mtm, background, geometry);
        let host = host.ok_or(ViewError::NoLayer)?;
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

/// The view that is the disc, and the layer of the view that hosts the
/// drawing. The drawing lives inside the background view, so that the glass
/// and the sectors are one thing: for glass it is the `contentView`, the
/// supported way to put content on top of the glass; its coordinates start at
/// the corner of the disc.
fn background_view(
    mtm: MainThreadMarker,
    background: Background,
    geometry: &MenuGeometry,
) -> (Retained<NSView>, Option<Retained<CALayer>>) {
    let side = disc_side(geometry);
    let outer = geometry.outer_radius();
    let frame = NSRect::new(NSPoint::new(MARGIN, MARGIN), NSSize::new(side, side));
    let local = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(side, side));
    match background {
        Background::Glass => {
            let overlay = NSView::initWithFrame(NSView::alloc(mtm), local);
            overlay.setWantsLayer(true);
            let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), frame);
            glass.setCornerRadius(outer);
            glass.setContentView(Some(&overlay));
            (Retained::into_super(glass), overlay.layer())
        }
        Background::Vibrancy => {
            let view = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
            view.setMaterial(NSVisualEffectMaterial::HUDWindow);
            view.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            view.setState(NSVisualEffectState::Active);
            view.setWantsLayer(true);
            if let Some(layer) = view.layer() {
                layer.setCornerRadius(outer);
                layer.setMasksToBounds(true);
            }
            let overlay = NSView::initWithFrame(NSView::alloc(mtm), local);
            overlay.setWantsLayer(true);
            view.addSubview(&overlay);
            (Retained::into_super(view), overlay.layer())
        }
        Background::Plain => {
            let view = NSView::initWithFrame(NSView::alloc(mtm), frame);
            view.setWantsLayer(true);
            let layer = view.layer();
            (view, layer)
        }
    }
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
        let glass_available = AnyClass::get(c"NSGlassEffectView").is_some();
        let (background, fell_back) = resolve_background(background, glass_available);
        if fell_back {
            info!("NSGlassEffectView is not available; the menu uses the vibrancy material");
        }
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
        let requested = NSPoint::new(model.center.x - side / 2.0, model.center.y - side / 2.0);
        let (dx, dy) = self.debug_shift;
        panel.window.setFrameOrigin(NSPoint::new(
            requested.x + f64::from(dx),
            requested.y + f64::from(dy),
        ));
        // Where AppKit put the window is what counts: the disc must sit at
        // the center of the model, not at the center of the window.
        let actual = panel.window.frame().origin;
        let shift = (snap(requested.x - actual.x), snap(requested.y - actual.y));
        let placement = place_disc(side, disc_side(&model.geometry), shift);
        if shift != (0.0, 0.0) {
            warn!(?shift, ?placement, "the window is not where it was put");
        }
        let scale = panel.window.backingScaleFactor();

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
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            panel
                .surface
                .reset(palette, available, shown_scale, opaque_disc);
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

/// Whether an appearance is the dark one. An unknown one counts as dark.
fn is_dark(appearance: &NSAppearance) -> bool {
    // SAFETY: AppKit constants, valid for the program's lifetime.
    let (aqua, dark_aqua) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names = NSArray::from_slice(&[aqua, dark_aqua]);
    appearance
        .bestMatchFromAppearancesWithNames(&names)
        .is_none_or(|name| name.isEqualToString(dark_aqua))
}

fn theme_of(dark: bool) -> Theme {
    if dark { Theme::Dark } else { Theme::Light }
}

fn app_theme(mtm: MainThreadMarker) -> Theme {
    theme_of(is_dark(
        &NSApplication::sharedApplication(mtm).effectiveAppearance(),
    ))
}

/// The theme of what is under the sectors. For glass that is the glass view;
/// the HUD material is pinned to the theme of the application so that it and
/// the palette agree; the plain disc and Reduce Transparency follow the
/// application.
fn surface_theme(
    mtm: MainThreadMarker,
    background: Background,
    disc: &NSView,
    reduce_transparency: bool,
) -> Theme {
    let app = app_theme(mtm);
    if reduce_transparency {
        return app;
    }
    match background {
        Background::Glass => theme_of(is_dark(&disc.effectiveAppearance())),
        Background::Vibrancy => {
            // SAFETY: AppKit constants, valid for the program's lifetime.
            let name = unsafe {
                if app == Theme::Dark {
                    NSAppearanceNameDarkAqua
                } else {
                    NSAppearanceNameAqua
                }
            };
            disc.setAppearance(NSAppearance::appearanceNamed(name).as_deref());
            app
        }
        Background::Plain => app,
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
    fn glass_is_used_when_the_system_has_it() {
        assert_eq!(resolve_background(None, true), (Background::Glass, false));
        assert_eq!(
            resolve_background(Some(Background::Glass), true),
            (Background::Glass, false)
        );
    }

    #[test]
    fn glass_falls_back_to_vibrancy_without_the_class() {
        assert_eq!(
            resolve_background(None, false),
            (Background::Vibrancy, true)
        );
        assert_eq!(
            resolve_background(Some(Background::Glass), false),
            (Background::Vibrancy, true)
        );
    }

    #[test]
    fn a_background_asked_for_is_kept() {
        for glass in [true, false] {
            assert_eq!(
                resolve_background(Some(Background::Vibrancy), glass),
                (Background::Vibrancy, false)
            );
            assert_eq!(
                resolve_background(Some(Background::Plain), glass),
                (Background::Plain, false)
            );
        }
    }

    #[test]
    fn backgrounds_are_named_for_the_debug_switch() {
        assert_eq!(Background::parse("glass"), Some(Background::Glass));
        assert_eq!(Background::parse("vibrancy"), Some(Background::Vibrancy));
        assert_eq!(Background::parse("plain"), Some(Background::Plain));
        assert_eq!(Background::parse("Glass"), None);
        assert_eq!(Background::parse(""), None);
    }

    #[test]
    fn rounding_noise_is_not_a_shift() {
        assert_eq!(snap(0.2), 0.0);
        assert_eq!(snap(-0.49), 0.0);
        assert_eq!(snap(0.5), 0.5);
        assert_eq!(snap(-6.0), -6.0);
    }
}
