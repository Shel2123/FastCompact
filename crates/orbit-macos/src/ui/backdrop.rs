//! What is behind the content of Orbit's windows: glass, the HUD material or
//! a plain translucent shape, and the theme it shows. The radial menu, the
//! toast and the info panel share it.

use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSApplication, NSGlassEffectView, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView,
};
use objc2_foundation::{NSArray, NSPoint, NSRect};
use tracing::info;

use super::palette::Theme;

/// What is behind the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    /// `NSGlassEffectView`, macOS 26 and later.
    Glass,
    /// `NSVisualEffectView` with the HUD material.
    Vibrancy,
    /// A plain translucent shape.
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

    /// The background to use when `requested` is asked for (a debug switch):
    /// `None` or glass on a system without `NSGlassEffectView` gets the best
    /// the system has.
    pub fn available(requested: Option<Self>) -> Self {
        let glass_available = AnyClass::get(c"NSGlassEffectView").is_some();
        let (background, fell_back) = resolve_background(requested, glass_available);
        if fell_back {
            info!("NSGlassEffectView is not available; the vibrancy material is used");
        }
        background
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

/// A background view and the view that holds the content on top of it.
pub struct Backdrop {
    /// The background; goes into the window.
    pub view: Retained<NSView>,
    /// Holds the content, layer-backed, with coordinates starting at the
    /// corner of the background. For glass it is the `contentView` of the
    /// glass, the supported way to put content on top of it; for the plain
    /// background it is `view` itself.
    pub content: Retained<NSView>,
}

impl Backdrop {
    /// A background of `background` at `frame`. Glass and the HUD material
    /// get rounded corners of `corner_radius`; the plain background is only
    /// a layer-backed view.
    pub fn new(
        mtm: MainThreadMarker,
        background: Background,
        frame: NSRect,
        corner_radius: f64,
    ) -> Self {
        let local = NSRect::new(NSPoint::new(0.0, 0.0), frame.size);
        match background {
            Background::Glass => {
                let content = NSView::initWithFrame(NSView::alloc(mtm), local);
                content.setWantsLayer(true);
                let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), frame);
                glass.setCornerRadius(corner_radius);
                glass.setContentView(Some(&content));
                Self {
                    view: Retained::into_super(glass),
                    content,
                }
            }
            Background::Vibrancy => {
                let view = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
                view.setMaterial(NSVisualEffectMaterial::HUDWindow);
                view.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
                view.setState(NSVisualEffectState::Active);
                view.setWantsLayer(true);
                if let Some(layer) = view.layer() {
                    layer.setCornerRadius(corner_radius);
                    layer.setMasksToBounds(true);
                }
                let content = NSView::initWithFrame(NSView::alloc(mtm), local);
                content.setWantsLayer(true);
                view.addSubview(&content);
                Self {
                    view: Retained::into_super(view),
                    content,
                }
            }
            // No fill and no mask: the caller draws the shape on the layer of
            // `content` (the menu draws its own disc).
            Background::Plain => {
                let view = NSView::initWithFrame(NSView::alloc(mtm), frame);
                view.setWantsLayer(true);
                Self {
                    content: view.clone(),
                    view,
                }
            }
        }
    }
}

/// Whether an appearance is the dark one. An unknown one counts as dark.
pub fn is_dark(appearance: &NSAppearance) -> bool {
    // SAFETY: AppKit constants, valid for the program's lifetime.
    let (aqua, dark_aqua) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names = NSArray::from_slice(&[aqua, dark_aqua]);
    appearance
        .bestMatchFromAppearancesWithNames(&names)
        .is_none_or(|name| name.isEqualToString(dark_aqua))
}

/// The theme for `dark`.
pub fn theme_of(dark: bool) -> Theme {
    if dark { Theme::Dark } else { Theme::Light }
}

/// The theme of the application.
pub fn app_theme(mtm: MainThreadMarker) -> Theme {
    theme_of(is_dark(
        &NSApplication::sharedApplication(mtm).effectiveAppearance(),
    ))
}

/// The theme of what is under the content. For glass that is the glass view;
/// the HUD material is pinned to the theme of the application so that it and
/// the colors of the content agree; the plain background and Reduce
/// Transparency follow the application.
pub fn surface_theme(
    mtm: MainThreadMarker,
    background: Background,
    view: &NSView,
    reduce_transparency: bool,
) -> Theme {
    let app = app_theme(mtm);
    if reduce_transparency {
        return app;
    }
    match background {
        Background::Glass => theme_of(is_dark(&view.effectiveAppearance())),
        Background::Vibrancy => {
            // SAFETY: AppKit constants, valid for the program's lifetime.
            let name = unsafe {
                if app == Theme::Dark {
                    NSAppearanceNameDarkAqua
                } else {
                    NSAppearanceNameAqua
                }
            };
            view.setAppearance(NSAppearance::appearanceNamed(name).as_deref());
            app
        }
        Background::Plain => app,
    }
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
}
