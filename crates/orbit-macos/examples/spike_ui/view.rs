//! Stage 5 spike: the radial menu as a layer tree in a non-activating panel.
//!
//! Everything expensive (panel, layers, paths, icons) is built once in
//! [`MenuPanel::new`]. [`MenuPanel::show`] and [`MenuPanel::highlight`] only
//! move the panel, swap colors of existing layers and add short animations.

use std::cell::Cell;
use std::f64::consts::FRAC_PI_2;
use std::ptr::{self, NonNull};

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSGlassEffectView, NSImage, NSPanel, NSPopUpMenuWindowLevel,
    NSScreen, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
    NSVisualEffectView, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGColor, CGImage, CGMutablePath};
use objc2_foundation::{NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CABasicAnimation, CALayer, CAMediaTiming, CAMediaTimingFunction, CAShapeLayer,
    CASpringAnimation, CATransaction,
};
use orbit_domain::action::{ActionId, CANCEL_SLOT, MENU_LAYOUT, MenuItem};
use orbit_domain::geometry::{Hit, MenuGeometry, ScreenPoint};
use tracing::{info, warn};

/// Side of the (square) panel, in points.
const SIZE: f64 = 240.0;
/// Icon box inside a sector, in points.
const ICON: f64 = 26.0;
/// Gap between neighbouring sectors, in radians.
const GAP: f64 = 0.02;

/// Background behind the sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    /// `NSGlassEffectView` (macOS 26).
    Glass,
    /// `NSVisualEffectView`, HUD material.
    Vibrancy,
    /// A plain translucent disc.
    Plain,
}

/// How the menu appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    /// `CASpringAnimation`, scale 0.92 to 1 and fade in.
    Spring,
    /// `CABasicAnimation` with ease-out, same values.
    Ease,
    /// No animation.
    None,
}

/// Settings read from the environment.
#[derive(Debug, Clone)]
pub struct Config {
    pub background: Background,
    pub appearance: Appearance,
    /// Slots drawn as unavailable.
    pub unavailable: Vec<usize>,
    pub bounce: f64,
    pub duration: f64,
    pub highlight_ms: f64,
}

pub struct MenuPanel {
    window: Retained<NSPanel>,
    glass: Option<Retained<NSView>>,
    plain: Retained<CAShapeLayer>,
    container: Retained<CALayer>,
    sectors: Vec<Retained<CAShapeLayer>>,
    _icons: Vec<Retained<CALayer>>,
    geometry: MenuGeometry,
    config: Config,
    available: Vec<bool>,
    center: Cell<ScreenPoint>,
    invoke: Cell<ScreenPoint>,
    selected: Cell<Option<usize>>,
}

impl MenuPanel {
    pub fn new(mtm: MainThreadMarker, config: Config) -> Self {
        let geometry = MenuGeometry::standard();
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIZE, SIZE));
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: the panel is owned by `MenuPanel`, not released on close.
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

        let content = NSView::initWithFrame(NSView::alloc(mtm), frame);
        content.setWantsLayer(true);
        window.setContentView(Some(&content));

        // Layers made by hand start at contentsScale 1 and blur on Retina.
        let scale = window.backingScaleFactor();
        let outer = geometry.outer_radius();
        let disc = NSRect::new(
            NSPoint::new(SIZE / 2.0 - outer, SIZE / 2.0 - outer),
            NSSize::new(outer * 2.0, outer * 2.0),
        );
        // The drawing lives in its own view, above the background. For glass
        // it is the glass view's `contentView`, the supported way to put
        // content on top of the glass; its coordinates start at the disc.
        let on_glass = config.background == Background::Glass;
        let overlay_frame = if on_glass {
            NSRect::new(NSPoint::new(0.0, 0.0), disc.size)
        } else {
            frame
        };
        let overlay = NSView::initWithFrame(NSView::alloc(mtm), overlay_frame);
        overlay.setWantsLayer(true);
        let glass = background_view(mtm, config.background, disc, outer, &overlay);
        if let Some(view) = &glass {
            content.addSubview(view);
        }
        if !on_glass {
            content.addSubview(&overlay);
        }
        // Centre of the menu in the coordinates of the overlay.
        let centre = if on_glass {
            NSPoint::new(outer, outer)
        } else {
            NSPoint::new(SIZE / 2.0, SIZE / 2.0)
        };
        let root = overlay.layer();
        let container = CALayer::new();
        container.setBounds(frame);
        container.setPosition(centre);
        container.setContentsScale(scale);

        // Fallback disc for Reduce Transparency and `plain`.
        let plain = CAShapeLayer::new();
        plain.setPath(Some(&disc_path(outer)));
        plain.setFillColor(Some(&srgb(0.12, 0.12, 0.14, 0.96)));
        plain.setContentsScale(scale);
        plain.setHidden(config.background != Background::Plain);
        container.addSublayer(&plain);

        let available: Vec<bool> = (0..geometry.slots())
            .map(|slot| !config.unavailable.contains(&slot))
            .collect();
        let mut sectors = Vec::with_capacity(geometry.slots());
        let mut icons = Vec::with_capacity(geometry.slots());
        for (slot, &is_available) in available.iter().enumerate() {
            let sector = CAShapeLayer::new();
            sector.setContentsScale(scale);
            sector.setPath(Some(&sector_path(&geometry, slot)));
            container.addSublayer(&sector);
            sectors.push(sector);
            let icon = icon_layer(&geometry, slot, is_available, scale);
            container.addSublayer(&icon);
            icons.push(icon);
        }
        if let Some(root) = root {
            root.addSublayer(&container);
        } else {
            warn!("panel content view has no layer");
        }

        let panel = Self {
            window,
            glass,
            plain,
            container,
            sectors,
            _icons: icons,
            geometry,
            config,
            available,
            center: Cell::new(ScreenPoint::new(0.0, 0.0)),
            invoke: Cell::new(ScreenPoint::new(0.0, 0.0)),
            selected: Cell::new(None),
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        panel.paint(None);
        CATransaction::commit();
        panel
    }

    /// Shows the panel for an invocation at `point` (Cocoa coordinates).
    /// Returns the center of the menu.
    pub fn show(&self, mtm: MainThreadMarker, point: NSPoint) -> NSPoint {
        let mut origin = NSPoint::new(point.x - SIZE / 2.0, point.y - SIZE / 2.0);
        if let Some(visible) = visible_frame_containing(mtm, point) {
            let max_x = visible.origin.x + visible.size.width - SIZE;
            let max_y = visible.origin.y + visible.size.height - SIZE;
            origin.x = origin.x.min(max_x).max(visible.origin.x);
            origin.y = origin.y.min(max_y).max(visible.origin.y);
        }
        let center = NSPoint::new(origin.x + SIZE / 2.0, origin.y + SIZE / 2.0);
        self.center.set(ScreenPoint::new(center.x, center.y));
        self.invoke.set(ScreenPoint::new(point.x, point.y));

        let workspace = NSWorkspace::sharedWorkspace();
        let reduce_motion = workspace.accessibilityDisplayShouldReduceMotion();
        let reduce_transparency = workspace.accessibilityDisplayShouldReduceTransparency();

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.selected.set(None);
        self.paint(None);
        if let Some(glass) = &self.glass {
            glass.setHidden(reduce_transparency);
        }
        self.plain
            .setHidden(!(reduce_transparency || self.config.background == Background::Plain));
        CATransaction::commit();

        self.window.setFrameOrigin(origin);
        self.container.removeAllAnimations();
        let appearance = if reduce_motion {
            Appearance::None
        } else {
            self.config.appearance
        };
        self.animate_in(appearance);
        self.window.orderFrontRegardless();
        center
    }

    pub fn hide(&self) {
        self.window.orderOut(None);
        self.container.removeAllAnimations();
    }

    /// Moves the highlight for a cursor position (Cocoa coordinates).
    /// Returns the slot now highlighted.
    pub fn track(&self, cursor: NSPoint) -> Option<usize> {
        let hit = self.geometry.hit(
            self.center.get(),
            self.invoke.get(),
            ScreenPoint::new(cursor.x, cursor.y),
        );
        let slot = match hit {
            Hit::Slot(slot) => Some(slot),
            Hit::Neutral | Hit::Outside => None,
        };
        if self.selected.replace(slot) != slot {
            CATransaction::begin();
            CATransaction::setAnimationDuration(self.config.highlight_ms / 1000.0);
            self.paint(slot);
            CATransaction::commit();
        }
        slot
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected.get()
    }

    pub fn available(&self, slot: usize) -> bool {
        self.available.get(slot).copied().unwrap_or(false)
    }

    /// Logs the layer tree, to compare with the SwiftUI reference.
    pub fn dump(&self) {
        dump_layer(&self.container, 0);
    }

    fn paint(&self, selected: Option<usize>) {
        for (slot, sector) in self.sectors.iter().enumerate() {
            let color = sector_color(slot, selected == Some(slot), self.available[slot]);
            sector.setFillColor(Some(&color));
        }
    }

    fn animate_in(&self, appearance: Appearance) {
        let fade = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str("opacity")));
        let scale =
            CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str("transform.scale")));
        let (from_opacity, from_scale) = (NSNumber::new_f64(0.0), NSNumber::new_f64(0.92));
        let one = NSNumber::new_f64(1.0);
        match appearance {
            Appearance::None => {}
            Appearance::Ease => {
                let timing = CAMediaTimingFunction::functionWithName(
                    // SAFETY: reading an immutable framework constant.
                    unsafe { objc2_quartz_core::kCAMediaTimingFunctionEaseOut },
                );
                for (animation, from) in [(&fade, &from_opacity), (&scale, &from_scale)] {
                    set_values(animation, from, &one);
                    animation.setDuration(self.config.duration);
                    animation.setTimingFunction(Some(&timing));
                    self.container.addAnimation_forKey(animation, None);
                }
            }
            Appearance::Spring => {
                set_values(&fade, &from_opacity, &one);
                fade.setDuration(self.config.duration.min(0.12));
                self.container.addAnimation_forKey(&fade, None);
                let spring = CASpringAnimation::initWithPerceptualDuration_bounce(
                    CASpringAnimation::alloc(),
                    self.config.duration,
                    self.config.bounce,
                );
                spring.setKeyPath(Some(&NSString::from_str("transform.scale")));
                set_values(&spring, &from_scale, &one);
                info!(
                    perceptual = spring.perceptualDuration(),
                    bounce = spring.bounce(),
                    settling = spring.settlingDuration(),
                    "spring animation"
                );
                self.container.addAnimation_forKey(&spring, None);
            }
        }
    }
}

/// Sets the numeric range of an animation.
fn set_values(animation: &CABasicAnimation, from: &NSNumber, to: &NSNumber) {
    // SAFETY: both values are NSNumbers, which Core Animation can interpolate
    // for the numeric key paths used here.
    unsafe {
        animation.setFromValue(Some(from));
        animation.setToValue(Some(to));
    }
}

fn background_view(
    mtm: MainThreadMarker,
    background: Background,
    disc: NSRect,
    outer: f64,
    overlay: &NSView,
) -> Option<Retained<NSView>> {
    match background {
        Background::Glass => {
            let view = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), disc);
            view.setCornerRadius(outer);
            view.setContentView(Some(overlay));
            Some(Retained::into_super(view))
        }
        Background::Vibrancy => {
            let view = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), disc);
            view.setMaterial(NSVisualEffectMaterial::HUDWindow);
            view.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            view.setState(NSVisualEffectState::Active);
            view.setWantsLayer(true);
            if let Some(layer) = view.layer() {
                layer.setCornerRadius(outer);
                layer.setMasksToBounds(true);
            }
            Some(Retained::into_super(view))
        }
        Background::Plain => None,
    }
}

fn disc_path(outer: f64) -> CFRetained<CGMutablePath> {
    let path = CGMutablePath::new();
    let rect = NSRect::new(
        NSPoint::new(SIZE / 2.0 - outer, SIZE / 2.0 - outer),
        NSSize::new(outer * 2.0, outer * 2.0),
    );
    // SAFETY: a null transform is allowed.
    unsafe { CGMutablePath::add_ellipse_in_rect(Some(&path), ptr::null(), rect) };
    path
}

/// Ring segment of `slot` between the neutral and the outer radius. Angles
/// of the geometry run clockwise from straight up; CoreGraphics measures
/// counter-clockwise from +x, so `cg = π/2 - angle`.
fn sector_path(geometry: &MenuGeometry, slot: usize) -> CFRetained<CGMutablePath> {
    let (start, end) = geometry.slot_bounds(slot);
    let (start, end) = (start + GAP / 2.0, end - GAP / 2.0);
    let (inner, outer) = (
        geometry.neutral_radius() + 2.0,
        geometry.outer_radius() - 2.0,
    );
    let c = SIZE / 2.0;
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

fn srgb(r: f64, g: f64, b: f64, a: f64) -> CFRetained<CGColor> {
    CGColor::new_srgb(r, g, b, a)
}

fn sector_color(slot: usize, highlighted: bool, available: bool) -> CFRetained<CGColor> {
    let cancel = slot == CANCEL_SLOT;
    match (highlighted, available) {
        (true, true) => {
            let base = if cancel {
                NSColor::systemRedColor()
            } else {
                NSColor::controlAccentColor()
            };
            let color = base.colorWithAlphaComponent(0.85).CGColor();
            // SAFETY: the pointer comes from a live reference and the retain
            // gives the new owner its own count.
            unsafe { CFRetained::retain(NonNull::from(&*color)) }
        }
        // The slot is under the cursor but cannot be chosen.
        (true, false) => srgb(0.5, 0.5, 0.5, 0.25),
        (false, true) if cancel => srgb(0.55, 0.15, 0.15, 0.30),
        (false, true) => srgb(1.0, 1.0, 1.0, 0.10),
        (false, false) => srgb(1.0, 1.0, 1.0, 0.04),
    }
}

/// SF Symbol of an item.
fn symbol_name(item: MenuItem) -> &'static str {
    match item {
        MenuItem::Cancel => "xmark",
        MenuItem::Action(ActionId::CopyPath) => "doc.on.doc",
        MenuItem::Action(ActionId::ShowInfo) => "info.circle",
        MenuItem::Action(ActionId::Rename) => "pencil",
        MenuItem::Action(ActionId::ArchiveZip) => "archivebox",
        MenuItem::Action(ActionId::ResizeImage) => "arrow.up.left.and.arrow.down.right",
        MenuItem::Action(ActionId::ConvertToPng) => "photo",
        MenuItem::Action(ActionId::ConvertToJpeg) => "photo.on.rectangle",
        MenuItem::Action(ActionId::Reveal) => "folder",
    }
}

/// An icon: a white layer masked by the symbol, so the symbol takes any color.
fn icon_layer(
    geometry: &MenuGeometry,
    slot: usize,
    available: bool,
    scale: f64,
) -> Retained<CALayer> {
    let angle = geometry.slot_center(slot);
    let radius = f64::midpoint(geometry.neutral_radius(), geometry.outer_radius());
    let c = SIZE / 2.0;
    let layer = CALayer::new();
    layer.setContentsScale(scale);
    layer.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(ICON, ICON)));
    layer.setPosition(NSPoint::new(
        c + radius * angle.sin(),
        c + radius * angle.cos(),
    ));
    layer.setBackgroundColor(Some(&srgb(
        1.0,
        1.0,
        1.0,
        if available { 0.95 } else { 0.30 },
    )));

    let name = symbol_name(MENU_LAYOUT[slot]);
    match symbol_image(name, scale) {
        Some((image, size)) => {
            let mask = CALayer::new();
            mask.setContentsScale(scale);
            mask.setFrame(NSRect::new(
                NSPoint::new((ICON - size.width) / 2.0, (ICON - size.height) / 2.0),
                size,
            ));
            // SAFETY: a CGImage is a valid `contents` value.
            unsafe { mask.setContents(Some(&*(&*image as *const CGImage as *const AnyObject))) };
            // SAFETY: the mask layer is not used as a mask elsewhere.
            unsafe { layer.setMask(Some(&mask)) };
        }
        None => warn!(name, slot, "symbol not found; icon left empty"),
    }
    layer
}

/// The symbol rendered once, at `scale` times its size, and its size in points.
fn symbol_image(name: &str, scale: f64) -> Option<(Retained<CGImage>, NSSize)> {
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        None,
    )?;
    let natural = image.size();
    let fit = (ICON - 6.0) / natural.width.max(natural.height).max(1.0);
    let size = NSSize::new(natural.width * fit, natural.height * fit);
    let mut rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(size.width * scale, size.height * scale),
    );
    // SAFETY: `rect` is a valid pointer for the duration of the call.
    let cg = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
    Some((cg, size))
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

fn dump_layer(layer: &CALayer, depth: usize) {
    let keys = layer.animationKeys();
    info!(
        depth,
        class = %layer.class().name().to_string_lossy(),
        frame = ?layer.frame(),
        opacity = layer.opacity(),
        animations = keys.map_or(0, |keys| keys.len()),
        "layer"
    );
    // SAFETY: the array is only read, on the main thread.
    if let Some(children) = unsafe { layer.sublayers() } {
        for child in children.iter() {
            dump_layer(&child, depth + 1);
        }
    }
}
