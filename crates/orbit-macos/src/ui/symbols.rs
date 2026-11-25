//! SF Symbols of the menu items.

use objc2::rc::Retained;
use objc2_app_kit::NSImage;
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use orbit_domain::action::{ActionId, MenuItem};

/// SF Symbol of an item.
pub const fn symbol_name(item: MenuItem) -> &'static str {
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

/// A symbol rendered once.
pub struct Symbol {
    /// The image, `scale` times the size in points.
    pub image: Retained<CGImage>,
    /// Size in points.
    pub size: NSSize,
}

/// The symbol `name` fitted into a square of `fit` points and rendered at
/// `scale` pixels per point, or `None` when the system has no such symbol or
/// cannot render it.
pub fn render(name: &str, fit: f64, scale: f64) -> Option<Symbol> {
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        None,
    )?;
    let natural = image.size();
    let factor = fit / natural.width.max(natural.height).max(1.0);
    let size = NSSize::new(natural.width * factor, natural.height * factor);
    let mut rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(size.width * scale, size.height * scale),
    );
    // SAFETY: `rect` is a valid pointer for the duration of the call.
    let image = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
    Some(Symbol { image, size })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use orbit_domain::action::MENU_LAYOUT;

    use super::*;

    #[test]
    fn every_item_has_its_own_symbol() {
        let names: Vec<_> = MENU_LAYOUT.iter().map(|item| symbol_name(*item)).collect();
        assert!(names.iter().all(|name| !name.is_empty()));
        let unique: HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), MENU_LAYOUT.len(), "names: {names:?}");
    }
}
