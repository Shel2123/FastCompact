//! Listen-only event tap on its own run loop thread. Records the location
//! stored in the hotkey's key-down event, which reflects the cursor at the
//! time of the key press even when the main thread handles it late, and the
//! mouse-downs and key-downs that the selection guard checks.
//!
//! Needs Input Monitoring access; without it the tap is not created.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Input that may change the Finder selection.
#[derive(Debug, Clone, Copy)]
#[expect(dead_code, reason = "fields are only logged through Debug")]
pub enum Input {
    MouseDown(Button),
    KeyDown { keycode: i64, autorepeat: bool },
}

#[derive(Debug, Clone, Copy)]
pub enum Button {
    Left,
    Right,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub struct TapInput {
    pub input: Input,
    pub seen_at: Instant,
}

/// Keep at most this many recent inputs.
const MAX_INPUTS: usize = 64;

static LAST_PRESS: Mutex<Option<TapPress>> = Mutex::new(None);
static INPUTS: Mutex<VecDeque<TapInput>> = Mutex::new(VecDeque::new());
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Takes the key-down recorded since the previous call, if any.
pub fn take_press() -> Option<TapPress> {
    LAST_PRESS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

/// Whether the tap is installed and enabled.
pub fn running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

/// Inputs seen after `since`, oldest first.
pub fn inputs_since(since: Instant) -> Vec<TapInput> {
    INPUTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|input| input.seen_at > since)
        .copied()
        .collect()
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
    let mask: CGEventMask = [
        CGEventType::KeyDown,
        CGEventType::LeftMouseDown,
        CGEventType::RightMouseDown,
        CGEventType::OtherMouseDown,
    ]
    .iter()
    .fold(0, |mask, event_type| mask | 1 << event_type.0);
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
    RUNNING.store(true, Ordering::Release);
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
    let seen_at = Instant::now();
    // SAFETY: the system passes a valid event for the duration of the call.
    let event_ref = unsafe { event.as_ref() };
    let input = match event_type {
        CGEventType::LeftMouseDown => Input::MouseDown(Button::Left),
        CGEventType::RightMouseDown => Input::MouseDown(Button::Right),
        CGEventType::OtherMouseDown => Input::MouseDown(Button::Other),
        _ => {
            let field = |field| CGEvent::integer_value_field(Some(event_ref), field);
            let keycode = field(CGEventField::KeyboardEventKeycode);
            let autorepeat = field(CGEventField::KeyboardEventAutorepeat) != 0;
            if keycode == SPACE_KEYCODE {
                // The hotkey itself and its autorepeats never count as input.
                let flags = CGEvent::flags(Some(event_ref));
                let wanted = CGEventFlags::MaskControl | CGEventFlags::MaskAlternate;
                if !autorepeat && flags.contains(wanted) {
                    let location = CGEvent::location(Some(event_ref));
                    let press = TapPress {
                        point: (location.x, location.y),
                        seen_at,
                    };
                    *LAST_PRESS.lock().unwrap_or_else(PoisonError::into_inner) = Some(press);
                }
                return event.as_ptr();
            }
            Input::KeyDown {
                keycode,
                autorepeat,
            }
        }
    };
    let mut inputs = INPUTS.lock().unwrap_or_else(PoisonError::into_inner);
    if inputs.len() == MAX_INPUTS {
        inputs.pop_front();
    }
    inputs.push_back(TapInput { input, seen_at });
    event.as_ptr()
}
