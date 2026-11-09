//! The layers of the radial menu.
//!
//! A [`Surface`] is the whole drawing: a plain disc, nine sector shapes and
//! nine icons, all built once. It owns no `NSView` or `NSWindow`; the panel
//! hosts its [`root`](Surface::root) inside the background view.
//!
//! Showing and moving the highlight only swap colors of existing layers.
//! Implicit animations are allowed in exactly one place, [`Surface::highlight`]:
//! every other change runs in a transaction that disables them, so that a
//! change cannot start an animation of its own.

use std::f64::consts::FRAC_PI_2;
use std::ptr;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGColor, CGImage, CGMutablePath};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CAShapeLayer, CATransaction};
use orbit_app::catalog::SLOT_COUNT;
use orbit_app::state::Highlight;
use orbit_domain::action::MENU_LAYOUT;
use orbit_domain::geometry::MenuGeometry;
use tracing::warn;

use super::palette::{Palette, Rgba, SlotState};
use super::symbols::{self, Symbol};

/// Icon box inside a sector, in points.
const ICON: f64 = 26.0;
/// Space between the symbol and the edge of the icon box, in points.
const ICON_PADDING: f64 = 6.0;
/// Gap between neighbouring sectors, in radians.
const GAP: f64 = 0.02;
/// Space between the disc and the edge of the window, in points. The window
/// is larger than the disc so that AppKit may move it a little and the disc
/// can follow (see [`place_disc`]).
pub const MARGIN: f64 = 10.0;
/// How many scales keep their rendered symbols: the display the menu was last
/// shown on and the one before it.
const CACHED_SCALES: usize = 2;

/// Whether the menu can be drawn with this geometry: one sector for each item
/// of the layout.
pub const fn drawable(geometry: &MenuGeometry) -> bool {
    geometry.slots() == SLOT_COUNT
}

/// Side of the square disc, in points.
pub const fn disc_side(geometry: &MenuGeometry) -> f64 {
    geometry.outer_radius() * 2.0
}

/// Side of the square window around the disc, in points.
pub const fn window_side(geometry: &MenuGeometry) -> f64 {
    disc_side(geometry) + MARGIN * 2.0
}

/// Where the disc goes inside the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiscPlacement {
    /// The origin of the disc in the window, in points.
    pub origin: (f64, f64),
    /// The part of the shift the disc could not follow without leaving the
    /// window. Zero unless AppKit moved the window by more than [`MARGIN`].
    pub residual: (f64, f64),
}

/// Where to put the disc so that its center stays where the model asked.
///
/// `shift` is the requested origin of the window minus the origin it got
/// (both Cocoa screen coordinates, y up): the window sits `shift` away from
/// where it should, so the disc moves by `shift` the other way inside it. The
/// disc stays inside the window, otherwise its edge would be cut off.
pub fn place_disc(window_side: f64, disc_side: f64, shift: (f64, f64)) -> DiscPlacement {
    let nominal = (window_side - disc_side) / 2.0;
    let furthest = window_side - disc_side;
    let axis = |shift: f64| {
        let wanted = nominal + shift;
        let origin = wanted.clamp(0.0, furthest);
        (origin, wanted - origin)
    };
    let (x, residual_x) = axis(shift.0);
    let (y, residual_y) = axis(shift.1);
    DiscPlacement {
        origin: (x, y),
        residual: (residual_x, residual_y),
    }
}

/// An icon: a layer filled with the ink color and masked by the symbol, so
/// that the symbol takes any color without being drawn again.
struct Icon {
    layer: Retained<CALayer>,
    mask: Retained<CALayer>,
}

/// The symbols rendered for the last scales.
#[derive(Default)]
struct SymbolCache {
    scales: Vec<(f64, Vec<Option<Symbol>>)>,
}

impl SymbolCache {
    /// The symbols at `scale`, rendered now if they were not yet.
    fn at(&mut self, scale: f64) -> &[Option<Symbol>] {
        let found = self
            .scales
            .iter()
            .position(|(cached, _)| (*cached - scale).abs() < f64::EPSILON);
        let index = found.unwrap_or_else(|| {
            let rendered = MENU_LAYOUT
                .iter()
                .enumerate()
                .map(|(slot, item)| {
                    let name = symbols::symbol_name(*item);
                    let symbol = symbols::render(name, ICON - ICON_PADDING, scale);
                    if symbol.is_none() {
                        warn!(name, slot, "symbol not found; icon left empty");
                    }
                    symbol
                })
                .collect();
            if self.scales.len() >= CACHED_SCALES {
                self.scales.remove(0);
            }
            self.scales.push((scale, rendered));
            self.scales.len() - 1
        });
        &self.scales[index].1
    }
}

/// The layers of the menu.
pub struct Surface {
    geometry: MenuGeometry,
    scale: f64,
    palette: Palette,
    available: [bool; SLOT_COUNT],
    root: Retained<CALayer>,
    plain: Retained<CAShapeLayer>,
    sectors: Vec<Retained<CAShapeLayer>>,
    icons: Vec<Icon>,
    symbols: SymbolCache,
}

impl Surface {
    /// Builds the layers for `geometry` at `scale` pixels per point. Returns
    /// `None` if the menu cannot be drawn with the geometry (see
    /// [`drawable`]).
    ///
    /// Colors are those of `palette`; [`reset`](Self::reset) sets them again
    /// for every show.
    pub fn build(geometry: &MenuGeometry, scale: f64, palette: Palette) -> Option<Self> {
        if !drawable(geometry) {
            return None;
        }
        let side = disc_side(geometry);
        let outer = geometry.outer_radius();
        let root = CALayer::new();
        root.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(side, side)));
        root.setPosition(NSPoint::new(outer, outer));
        root.setContentsScale(scale);

        let plain = CAShapeLayer::new();
        plain.setPath(Some(&disc_path(outer)));
        plain.setContentsScale(scale);
        plain.setHidden(true);
        root.addSublayer(&plain);

        let mut sectors = Vec::with_capacity(SLOT_COUNT);
        let mut icons = Vec::with_capacity(SLOT_COUNT);
        for slot in 0..SLOT_COUNT {
            let sector = CAShapeLayer::new();
            sector.setContentsScale(scale);
            sector.setPath(Some(&sector_path(geometry, slot)));
            root.addSublayer(&sector);
            sectors.push(sector);
            let icon = icon_layer(geometry, slot, scale);
            root.addSublayer(&icon.layer);
            icons.push(icon);
        }
        let mut surface = Self {
            geometry: *geometry,
            scale,
            palette,
            available: [true; SLOT_COUNT],
            root,
            plain,
            sectors,
            icons,
            symbols: SymbolCache::default(),
        };
        surface.apply_symbols();
        Some(surface)
    }

    /// The geometry the layers were built for.
    pub const fn geometry(&self) -> &MenuGeometry {
        &self.geometry
    }

    /// The layer to host; its bounds are the disc and its center is the
    /// center of the menu.
    pub fn root(&self) -> &CALayer {
        &self.root
    }

    /// Prepares the layers for a show: nothing highlighted, colors of
    /// `palette`, slots drawn available or not, and the opaque disc under the
    /// sectors when `opaque_disc` (Reduce Transparency, plain background).
    /// `scale` is the scale of the screen the menu is shown on; when it
    /// differs from the last one the icons are rendered again.
    ///
    /// Any transition left from the last show is removed, and none starts: the
    /// whole change runs with implicit animations disabled.
    pub fn reset(
        &mut self,
        palette: Palette,
        available: [bool; SLOT_COUNT],
        scale: f64,
        opaque_disc: bool,
    ) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.root.removeAllAnimations();
        self.plain.removeAllAnimations();
        for (sector, icon) in self.sectors.iter().zip(&self.icons) {
            sector.removeAllAnimations();
            icon.layer.removeAllAnimations();
            icon.mask.removeAllAnimations();
        }
        self.palette = palette;
        self.available = available;
        if (scale - self.scale).abs() >= f64::EPSILON {
            self.set_scale(scale);
        }
        self.plain.setFillColor(Some(&cg_color(palette.disc())));
        self.plain.setHidden(!opaque_disc);
        for slot in 0..SLOT_COUNT {
            self.paint(slot, false);
        }
        CATransaction::commit();
    }

    /// Moves the highlight from `from` to `to`, repainting only their slots,
    /// with a transition of `seconds`. The only call that animates.
    pub fn highlight(&mut self, from: Highlight, to: Highlight, seconds: f64) {
        CATransaction::begin();
        CATransaction::setDisableActions(false);
        CATransaction::setAnimationDuration(seconds);
        for highlight in [from, to] {
            if let Highlight::Active(slot) | Highlight::Inactive(slot) = highlight
                && slot < SLOT_COUNT
            {
                self.paint(slot, highlight == to);
            }
        }
        CATransaction::commit();
    }

    /// How many layers have an animation. Zero after [`reset`](Self::reset).
    pub fn animated_layers(&self) -> usize {
        let animated = |layer: &CALayer| layer.animationKeys().is_some_and(|keys| !keys.is_empty());
        let mut count = usize::from(animated(&self.root)) + usize::from(animated(&self.plain));
        for sector in &self.sectors {
            count += usize::from(animated(sector));
        }
        for icon in &self.icons {
            count += usize::from(animated(&icon.layer)) + usize::from(animated(&icon.mask));
        }
        count
    }

    fn paint(&self, slot: usize, highlighted: bool) {
        let state = SlotState {
            highlighted,
            available: self.available[slot],
        };
        self.sectors[slot].setFillColor(Some(&cg_color(self.palette.sector(slot, state))));
        self.icons[slot]
            .layer
            .setBackgroundColor(Some(&cg_color(self.palette.icon(slot, state))));
    }

    fn set_scale(&mut self, scale: f64) {
        self.scale = scale;
        self.root.setContentsScale(scale);
        self.plain.setContentsScale(scale);
        for (sector, icon) in self.sectors.iter().zip(&self.icons) {
            sector.setContentsScale(scale);
            icon.layer.setContentsScale(scale);
            icon.mask.setContentsScale(scale);
        }
        self.apply_symbols();
    }

    /// Puts the symbols of the current scale into the icon masks.
    fn apply_symbols(&mut self) {
        let symbols = self.symbols.at(self.scale);
        for (icon, symbol) in self.icons.iter().zip(symbols) {
            match symbol {
                Some(symbol) => {
                    icon.mask.setFrame(NSRect::new(
                        NSPoint::new(
                            (ICON - symbol.size.width) / 2.0,
                            (ICON - symbol.size.height) / 2.0,
                        ),
                        symbol.size,
                    ));
                    // SAFETY: a CGImage is a valid `contents` value.
                    unsafe {
                        icon.mask.setContents(Some(
                            &*(&*symbol.image as *const CGImage as *const AnyObject),
                        ));
                    }
                }
                None => {
                    // SAFETY: `None` clears the contents.
                    unsafe { icon.mask.setContents(None) };
                }
            }
        }
    }

    /// The symbols found at the current scale (for tests).
    #[cfg(test)]
    fn symbols_found(&mut self) -> usize {
        self.symbols.at(self.scale).iter().flatten().count()
    }
}

fn cg_color(color: Rgba) -> CFRetained<CGColor> {
    CGColor::new_srgb(color.r, color.g, color.b, color.a)
}

fn disc_path(outer: f64) -> CFRetained<CGMutablePath> {
    let path = CGMutablePath::new();
    let rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(outer * 2.0, outer * 2.0),
    );
    // SAFETY: a null transform is allowed.
    unsafe { CGMutablePath::add_ellipse_in_rect(Some(&path), ptr::null(), rect) };
    path
}

/// Ring segment of `slot` between the neutral and the outer radius. Angles
/// of the geometry run clockwise from straight up; CoreGraphics measures
/// counter-clockwise from +x, so `cg = π/2 - angle`. The path is in the
/// coordinates of the root layer, whose center is `(outer, outer)`.
fn sector_path(geometry: &MenuGeometry, slot: usize) -> CFRetained<CGMutablePath> {
    let (start, end) = geometry.slot_bounds(slot);
    let (start, end) = (start + GAP / 2.0, end - GAP / 2.0);
    let (inner, outer) = (
        geometry.neutral_radius() + 2.0,
        geometry.outer_radius() - 2.0,
    );
    let c = geometry.outer_radius();
    let point = |radius: f64, angle: f64| (c + radius * angle.sin(), c + radius * angle.cos());
    let path = CGMutablePath::new();
    let (sx, sy) = point(outer, start);
    let (ex, ey) = point(inner, end);
    // SAFETY: null transforms are allowed.
    unsafe {
        CGMutablePath::move_to_point(Some(&path), ptr::null(), sx, sy);
        CGMutablePath::add_arc(
            Some(&path),
            ptr::null(),
            c,
            c,
            outer,
            FRAC_PI_2 - start,
            FRAC_PI_2 - end,
            true,
        );
        CGMutablePath::add_line_to_point(Some(&path), ptr::null(), ex, ey);
        CGMutablePath::add_arc(
            Some(&path),
            ptr::null(),
            c,
            c,
            inner,
            FRAC_PI_2 - end,
            FRAC_PI_2 - start,
            false,
        );
    }
    CGMutablePath::close_subpath(Some(&path));
    path
}

/// The icon of `slot` at the middle radius of the ring. The symbol is put
/// into the mask by [`Surface::apply_symbols`].
fn icon_layer(geometry: &MenuGeometry, slot: usize, scale: f64) -> Icon {
    let angle = geometry.slot_center(slot);
    let radius = f64::midpoint(geometry.neutral_radius(), geometry.outer_radius());
    let c = geometry.outer_radius();
    let layer = CALayer::new();
    layer.setContentsScale(scale);
    layer.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(ICON, ICON)));
    layer.setPosition(NSPoint::new(
        c + radius * angle.sin(),
        c + radius * angle.cos(),
    ));
    let mask = CALayer::new();
    mask.setContentsScale(scale);
    // SAFETY: the mask layer is not used as a mask elsewhere.
    unsafe { layer.setMask(Some(&mask)) };
    Icon { layer, mask }
}

#[cfg(test)]
mod tests {
    use objc2_foundation::NSString;
    use objc2_quartz_core::{CABasicAnimation, CAMediaTiming};

    use super::super::palette::Theme;
    use super::*;

    const BLUE: Rgba = Rgba::new(0.0, 0.478, 1.0, 1.0);
    const RED: Rgba = Rgba::new(1.0, 0.231, 0.188, 1.0);

    fn palette(theme: Theme) -> Palette {
        Palette::new(theme, BLUE, RED)
    }

    fn surface(scale: f64) -> Surface {
        let built = Surface::build(&MenuGeometry::standard(), scale, palette(Theme::Dark));
        let Some(surface) = built else {
            panic!("the standard geometry is drawable");
        };
        surface
    }

    /// The components of a color, as stored.
    fn components(color: &CGColor) -> Vec<f64> {
        let count = CGColor::number_of_components(Some(color));
        let pointer = CGColor::components(Some(color));
        // SAFETY: Core Graphics returns `count` values that live as long as
        // the color.
        (0..count)
            .map(|index| unsafe { *pointer.add(index) })
            .collect()
    }

    fn assert_color(color: Option<Retained<CGColor>>, expected: Rgba, what: &str) {
        let color = color.unwrap_or_else(|| panic!("{what}: no color"));
        let got = components(&color);
        let want = [expected.r, expected.g, expected.b, expected.a];
        assert_eq!(got.len(), 4, "{what}: {got:?}");
        for (got, want) in got.iter().zip(want) {
            assert!((got - want).abs() < 1e-6, "{what}: {got:?} vs {want:?}");
        }
    }

    fn all_available() -> [bool; SLOT_COUNT] {
        [true; SLOT_COUNT]
    }

    #[test]
    fn disc_sits_in_the_middle_of_the_window_without_a_shift() {
        let placement = place_disc(240.0, 220.0, (0.0, 0.0));
        assert_eq!(placement.origin, (MARGIN, MARGIN));
        assert_eq!(placement.residual, (0.0, 0.0));
    }

    #[test]
    fn disc_follows_a_shift_on_each_axis() {
        // The window sits 6 left and 4 above where it should: the disc moves
        // 6 right and 4 down inside it.
        let both = place_disc(240.0, 220.0, (6.0, -4.0));
        assert_eq!(both.origin, (16.0, 6.0));
        assert_eq!(both.residual, (0.0, 0.0));
        let only_x = place_disc(240.0, 220.0, (-8.0, 0.0));
        assert_eq!(only_x.origin, (2.0, 10.0));
        let only_y = place_disc(240.0, 220.0, (0.0, 8.0));
        assert_eq!(only_y.origin, (10.0, 18.0));
        // The disc stays inside the window.
        for placement in [both, only_x, only_y] {
            assert!(placement.origin.0 >= 0.0 && placement.origin.0 + 220.0 <= 240.0);
            assert!(placement.origin.1 >= 0.0 && placement.origin.1 + 220.0 <= 240.0);
        }
    }

    #[test]
    fn disc_does_not_leave_the_window_and_reports_what_is_left() {
        let placement = place_disc(240.0, 220.0, (14.0, -13.0));
        // The window has 20 points to give: 10 each way from the middle.
        assert_eq!(placement.origin, (20.0, 0.0));
        assert_eq!(placement.residual, (4.0, -3.0));
    }

    #[test]
    fn window_is_larger_than_the_disc_by_the_margin() {
        let geometry = MenuGeometry::standard();
        assert_eq!(disc_side(&geometry), 220.0);
        assert_eq!(window_side(&geometry), 240.0);
    }

    #[test]
    fn only_a_menu_with_a_sector_per_item_is_drawable() {
        assert!(drawable(&MenuGeometry::standard()));
        let Ok(five) = MenuGeometry::new(5, 28.0, 110.0, 6.0, 0.0) else {
            panic!("valid geometry");
        };
        assert!(!drawable(&five));
        assert!(Surface::build(&five, 2.0, palette(Theme::Dark)).is_none());
    }

    #[test]
    fn layers_are_built_at_the_given_scale() {
        for scale in [1.0, 2.0] {
            let surface = surface(scale);
            assert_eq!(surface.root.contentsScale(), scale);
            assert_eq!(surface.plain.contentsScale(), scale);
            assert_eq!(surface.sectors.len(), SLOT_COUNT);
            assert_eq!(surface.icons.len(), SLOT_COUNT);
            for sector in &surface.sectors {
                assert_eq!(sector.contentsScale(), scale);
            }
            for icon in &surface.icons {
                assert_eq!(icon.layer.contentsScale(), scale);
                assert_eq!(icon.mask.contentsScale(), scale);
            }
        }
    }

    #[test]
    fn every_symbol_is_found() {
        let mut surface = surface(2.0);
        assert_eq!(surface.symbols_found(), SLOT_COUNT);
    }

    #[test]
    fn reset_paints_every_slot_for_the_theme() {
        let mut surface = surface(2.0);
        let mut available = all_available();
        available[5] = false;
        surface.reset(palette(Theme::Light), available, 2.0, false);
        let light = palette(Theme::Light);
        let idle = SlotState {
            highlighted: false,
            available: true,
        };
        let off = SlotState {
            highlighted: false,
            available: false,
        };
        assert_color(
            surface.sectors[0].fillColor(),
            light.sector(0, idle),
            "idle",
        );
        assert_color(surface.sectors[5].fillColor(), light.sector(5, off), "off");
        assert_color(
            surface.icons[0].layer.backgroundColor(),
            light.icon(0, idle),
            "icon",
        );
        assert_color(
            surface.icons[5].layer.backgroundColor(),
            light.icon(5, off),
            "off icon",
        );
    }

    #[test]
    fn reset_shows_the_opaque_disc_only_when_asked() {
        let mut surface = surface(2.0);
        surface.reset(palette(Theme::Dark), all_available(), 2.0, true);
        assert!(!surface.plain.isHidden());
        assert_color(
            surface.plain.fillColor(),
            palette(Theme::Dark).disc(),
            "disc",
        );
        surface.reset(palette(Theme::Dark), all_available(), 2.0, false);
        assert!(surface.plain.isHidden());
    }

    #[test]
    fn reset_follows_a_change_of_scale() {
        let mut surface = surface(1.0);
        surface.reset(palette(Theme::Dark), all_available(), 2.0, false);
        assert_eq!(surface.root.contentsScale(), 2.0);
        for icon in &surface.icons {
            assert_eq!(icon.layer.contentsScale(), 2.0);
            assert_eq!(icon.mask.contentsScale(), 2.0);
        }
        assert_eq!(surface.symbols_found(), SLOT_COUNT);
        // The symbols of both scales are kept.
        assert_eq!(surface.symbols.scales.len(), 2);
    }

    #[test]
    fn highlight_repaints_the_two_slots_it_moves_between() {
        let mut surface = surface(2.0);
        surface.reset(palette(Theme::Dark), all_available(), 2.0, false);
        let dark = palette(Theme::Dark);
        let hovered = SlotState {
            highlighted: true,
            available: true,
        };
        let idle = SlotState {
            highlighted: false,
            available: true,
        };
        surface.highlight(Highlight::None, Highlight::Active(1), 0.07);
        assert_color(
            surface.sectors[1].fillColor(),
            dark.sector(1, hovered),
            "on",
        );
        assert_color(
            surface.icons[1].layer.backgroundColor(),
            dark.icon(1, hovered),
            "on icon",
        );
        surface.highlight(Highlight::Active(1), Highlight::Active(2), 0.07);
        assert_color(surface.sectors[1].fillColor(), dark.sector(1, idle), "off");
        assert_color(
            surface.sectors[2].fillColor(),
            dark.sector(2, hovered),
            "next",
        );
    }

    #[test]
    fn show_during_an_unfinished_highlight_leaves_no_animation() {
        let mut surface = surface(2.0);
        surface.reset(palette(Theme::Dark), all_available(), 2.0, false);
        // A transition of the last show that has not finished: an explicit
        // animation stands in for it, so that no window is needed.
        // It lasts a minute and is not removed when it ends, so a commit of the
        // implicit transaction of this thread cannot finish it before the check.
        let unfinished =
            CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str("fillColor")));
        unfinished.setDuration(60.0);
        unfinished.setRemovedOnCompletion(false);
        let key = NSString::from_str("unfinished");
        surface.sectors[1].addAnimation_forKey(&unfinished, Some(&key));
        surface.icons[3]
            .layer
            .addAnimation_forKey(&unfinished, Some(&key));
        assert_eq!(surface.animated_layers(), 2);

        surface.reset(palette(Theme::Light), all_available(), 2.0, false);

        assert_eq!(surface.animated_layers(), 0);
        let light = palette(Theme::Light);
        let idle = SlotState {
            highlighted: false,
            available: true,
        };
        assert_color(
            surface.sectors[1].fillColor(),
            light.sector(1, idle),
            "sector",
        );
        assert_color(
            surface.icons[3].layer.backgroundColor(),
            light.icon(3, idle),
            "icon",
        );
    }
}
