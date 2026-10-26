//! The Orbit item in the menu bar.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength};
use objc2_foundation::ns_string;

/// Adds the Orbit item to the menu bar, with a menu to quit. The item stays
/// while the returned value is alive.
pub fn status_item(mtm: MainThreadMarker) -> Retained<NSStatusItem> {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        button.setTitle(ns_string!("◎"));
    }
    let menu = NSMenu::new(mtm);
    // SAFETY: `terminate:` is an NSApplication action; with no target it goes
    // up the responder chain to NSApp.
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Quit Orbit"),
            Some(sel!(terminate:)),
            ns_string!("q"),
        )
    };
    menu.addItem(&quit);
    item.setMenu(Some(&menu));
    item
}
