//! Listen-only event tap on its own run loop thread. Records the location
//! stored in the hotkey's key-down event, which reflects the cursor at the
//! time of the key press even when the main thread handles it late.
//!
//! Needs Input Monitoring access; without it the tap is not created.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Instant;

use objc2_core_foundation::{CFMachPort, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGPreflightListenEventAccess,
    CGRequestListenEventAccess,
};
use tracing::{error, info, warn};

/// Virtual key code of Space.
const SPACE_KEYCODE: i64 = 0x31;

#[derive(Debug, Clone, Copy)]
pub struct TapPress {
    /// Quartz global coordinates from the event itself.
    pub point: (f64, f64),
    pub seen_at: Instant,
}

static LAST_PRESS: Mutex<Option<TapPress>> = Mutex::new(None);

/// Takes the key-down recorded since the previous call, if any.
pub fn take_press() -> Option<TapPress> {
    LAST_PRESS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

pub fn spawn() {
    if !CGPreflightListenEventAccess() {
        warn!(
            requested = CGRequestListenEventAccess(),
            "input monitoring not granted yet"
        );
    }
    let spawned = thread::Builder::new()
        .name("orbit-spike-tap".into())
        .spawn(run_tap);
    if let Err(err) = spawned {
        error!(%err, "event tap thread not started");
    }
}

fn run_tap() {
    let mask: CGEventMask = 1 << CGEventType::KeyDown.0;
    // SAFETY: the callback matches CGEventTapCallBack and ignores user_info.
    let port: Option<objc2_core_foundation::CFRetained<CFMachPort>> = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            mask,
            Some(on_event),
            std::ptr::null_mut(),
        )
    };
    let Some(port) = port else {
        error!("event tap not created (Input Monitoring access missing?)");
        return;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        error!("event tap run loop source not created");
        return;
    };
    let Some(run_loop) = CFRunLoop::current() else {
        error!("no run loop for the tap thread");
        return;
    };
    // SAFETY: reading an immutable framework constant.
    let modes = unsafe { kCFRunLoopCommonModes };
    run_loop.add_source(Some(&source), modes);
    CGEvent::tap_enable(&port, true);
    info!("event tap running");
    CFRunLoop::run();
}

unsafe extern "C-unwind" fn on_event(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    if event_type == CGEventType::TapDisabledByTimeout
        || event_type == CGEventType::TapDisabledByUserInput
    {
        warn!(?event_type, "event tap disabled");
        return event.as_ptr();
    }
    // SAFETY: the system passes a valid event for the duration of the call.
    let event_ref = unsafe { event.as_ref() };
    let keycode = CGEvent::integer_value_field(Some(event_ref), CGEventField::KeyboardEventKeycode);
    let flags = CGEvent::flags(Some(event_ref));
    let wanted = CGEventFlags::MaskControl | CGEventFlags::MaskAlternate;
    if keycode == SPACE_KEYCODE && flags.contains(wanted) {
        let location = CGEvent::location(Some(event_ref));
        let press = TapPress {
            point: (location.x, location.y),
            seen_at: Instant::now(),
        };
        *LAST_PRESS.lock().unwrap_or_else(PoisonError::into_inner) = Some(press);
    }
    event.as_ptr()
}
