//! System permission checks.
//!
//! The event tap is active, so it needs Accessibility; Input Monitoring is
//! not needed and does not create an active tap on its own (spike F9).
//! Accessibility granted while Orbit runs applies without a restart.

use objc2_application_services::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString};

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

/// Whether Orbit has Accessibility access. With `prompt`, the system asks
/// the user to grant it if it is missing.
///
/// Can take tens of milliseconds; keep it off the tap thread.
pub fn accessibility_trusted(prompt: bool) -> bool {
    if !prompt {
        // SAFETY: takes no arguments and only reads the trust state.
        return unsafe { AXIsProcessTrusted() };
    }
    // SAFETY: reading an immutable framework constant.
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let options: CFRetained<CFDictionary<CFString, CFBoolean>> =
        CFDictionary::from_slices(&[key], &[CFBoolean::new(true)]);
    // SAFETY: the options dictionary has CFString keys and CFBoolean values,
    // as the function expects.
    unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) }
}

/// Whether secure keyboard entry is on, in any process. The tap then gets
/// no keyboard events.
pub fn secure_input_enabled() -> bool {
    // SAFETY: takes no arguments and only reads a system flag.
    unsafe { IsSecureEventInputEnabled() != 0 }
}
