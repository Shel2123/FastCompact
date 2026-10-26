//! Listen-only event tap on its own run loop thread. Records the location
//! stored in the hotkey's key-down event, which reflects the cursor at the
//! time of the key press even when the main thread handles it late, and the
//! mouse-downs and key-downs that the selection guard checks.
//!
//! Every event the tap sees gets a sequence number in the order the tap
//! handles it. The fence (stage 4, F2/F3) posts a marker event after the
//! selection read and waits until the tap handles it: if delivery to the tap
//! is in order, every input that reached Finder before the read ended has a
//! smaller number than the marker.
//!
//! Needs Input Monitoring access; without it the tap is not created.

use std::cell::OnceCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMouseButton, CGPreflightListenEventAccess,
    CGRequestListenEventAccess, CGScrollEventUnit,
};
use tracing::{error, info, warn};

/// Virtual key code of Space.
const SPACE_KEYCODE: i64 = 0x31;
/// Virtual key code of Esc.
const ESCAPE_KEYCODE: i64 = 0x35;

#[derive(Debug, Clone, Copy)]
pub struct TapPress {
    /// Quartz global coordinates from the event itself.
    pub point: (f64, f64),
    pub seen_at: Instant,
    /// Sequence number of the key-down in the tap's event order.
    pub seq: u64,
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
    pub seq: u64,
}

/// Event used as the fence marker (scenario F2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceKind {
    /// Scroll wheel event with zero deltas.
    Scroll,
    /// Mouse-moved event to the current cursor position.
    Move,
    /// `kCGEventNull`.
    Null,
}

impl FenceKind {
    fn event_type(self) -> CGEventType {
        match self {
            Self::Scroll => CGEventType::ScrollWheel,
            Self::Move => CGEventType::MouseMoved,
            Self::Null => CGEventType::Null,
        }
    }
}

/// State of the tap when it handled a fence marker.
#[derive(Debug, Clone, Copy)]
pub struct FenceSnapshot {
    /// Sequence number of the marker.
    pub seq: u64,
    /// Newest input before the marker; 0 when there was none.
    pub last_input_seq: u64,
    /// Newest tap interruption before the marker; 0 when there was none.
    pub last_interrupt_seq: u64,
    /// From posting the marker to the tap handling it.
    #[expect(dead_code, reason = "only logged through Debug")]
    pub latency: Duration,
}

/// Keep at most this many recent inputs.
const MAX_INPUTS: usize = 64;
/// Keep at most this many handled fences.
const MAX_FENCES: usize = 16;
/// Fence nonces start here, so that the user data of other synthetic events
/// is not taken for a marker.
const FENCE_NONCE_BASE: i64 = 0x0B17_0000_0000;

static LAST_PRESS: Mutex<Option<TapPress>> = Mutex::new(None);
static INPUTS: Mutex<VecDeque<TapInput>> = Mutex::new(VecDeque::new());
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Sequence number of the last event the tap handled.
static SEQ: AtomicU64 = AtomicU64::new(0);
static LAST_INPUT_SEQ: AtomicU64 = AtomicU64::new(0);
static LAST_INTERRUPT_SEQ: AtomicU64 = AtomicU64::new(0);
/// Delay the tap applies before recording an input (scenario F3).
static INPUT_DELAY_MS: AtomicU64 = AtomicU64::new(0);
static NEXT_NONCE: AtomicI64 = AtomicI64::new(FENCE_NONCE_BASE);
/// Posted fences that the tap has not handled yet, with their post time.
static PENDING_FENCES: Mutex<Vec<(i64, Instant)>> = Mutex::new(Vec::new());
static FENCES: Mutex<VecDeque<(i64, FenceSnapshot)>> = Mutex::new(VecDeque::new());
static FENCE_SEEN: Condvar = Condvar::new();

thread_local! {
    /// The tap's port, to enable it again after the system disables it.
    static PORT: OnceCell<CFRetained<CFMachPort>> = const { OnceCell::new() };
}

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

/// Inputs with sequence numbers in `after+1..before`, oldest first.
pub fn inputs_between(after: u64, before: u64) -> Vec<TapInput> {
    INPUTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|input| input.seq > after && input.seq < before)
        .copied()
        .collect()
}

/// Posts a fence marker and returns its nonce.
pub fn post_fence(kind: FenceKind) -> Result<i64, String> {
    let event = match kind {
        FenceKind::Scroll => {
            CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 1, 0, 0, 0)
        }
        FenceKind::Move => {
            let current = CGEvent::new(None);
            let location = CGEvent::location(current.as_deref());
            CGEvent::new_mouse_event(None, CGEventType::MouseMoved, location, CGMouseButton::Left)
        }
        FenceKind::Null => {
            let event = CGEvent::new(None);
            CGEvent::set_type(event.as_deref(), CGEventType::Null);
            event
        }
    };
    let Some(event) = event else {
        return Err(format!("{kind:?} marker event not created"));
    };
    let nonce = NEXT_NONCE.fetch_add(1, Ordering::Relaxed);
    CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, nonce);
    PENDING_FENCES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((nonce, Instant::now()));
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    Ok(nonce)
}

/// Waits until the tap handles the fence `nonce`, at most `timeout`.
pub fn wait_fence(nonce: i64, timeout: Duration) -> Option<FenceSnapshot> {
    let deadline = Instant::now() + timeout;
    let mut fences = FENCES.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if let Some((_, snapshot)) = fences.iter().find(|(seen, _)| *seen == nonce) {
            return Some(*snapshot);
        }
        let left = deadline.checked_duration_since(Instant::now())?;
        fences = FENCE_SEEN
            .wait_timeout(fences, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

/// Starts the tap thread. `input_delay` makes the tap wait that long before
/// recording each mouse-down or key-down (scenario F3); `fence` adds the
/// marker's event type to the tap mask.
pub fn spawn(input_delay: Duration, fence: Option<FenceKind>) {
    if !CGPreflightListenEventAccess() {
        warn!(
            requested = CGRequestListenEventAccess(),
            "input monitoring not granted yet"
        );
    }
    let delay_ms = u64::try_from(input_delay.as_millis()).unwrap_or(u64::MAX);
    INPUT_DELAY_MS.store(delay_ms, Ordering::Relaxed);
    let spawned = thread::Builder::new()
        .name("orbit-spike-tap".into())
        .spawn(move || run_tap(fence));
    if let Err(err) = spawned {
        error!(%err, "event tap thread not started");
    }
}

fn run_tap(fence: Option<FenceKind>) {
    let mut types = vec![
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
        CGEventType::LeftMouseDown,
        CGEventType::RightMouseDown,
        CGEventType::OtherMouseDown,
    ];
    if let Some(kind) = fence {
        types.push(kind.event_type());
    }
    let mask: CGEventMask = types
        .iter()
        .fold(0, |mask, event_type| mask | 1 << event_type.0);
    // SAFETY: the callback matches CGEventTapCallBack and ignores user_info.
    let port: Option<CFRetained<CFMachPort>> = unsafe {
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
    PORT.with(|cell| {
        if cell.set(port).is_err() {
            warn!("tap port was already stored");
        }
    });
    RUNNING.store(true, Ordering::Release);
    info!(
        ?fence,
        input_delay_ms = INPUT_DELAY_MS.load(Ordering::Relaxed),
        "event tap running"
    );
    CFRunLoop::run();
}

unsafe extern "C-unwind" fn on_event(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    let seen_at = Instant::now();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    if event_type == CGEventType::TapDisabledByTimeout
        || event_type == CGEventType::TapDisabledByUserInput
    {
        LAST_INTERRUPT_SEQ.store(seq, Ordering::Relaxed);
        warn!(seq, ?event_type, "event tap disabled; enabling it again");
        PORT.with(|cell| match cell.get() {
            Some(port) => CGEvent::tap_enable(port, true),
            None => error!("tap port unknown; the tap stays disabled"),
        });
        return event.as_ptr();
    }
    // SAFETY: the system passes a valid event for the duration of the call.
    let event_ref = unsafe { event.as_ref() };
    let field = |field| CGEvent::integer_value_field(Some(event_ref), field);

    let user_data = field(CGEventField::EventSourceUserData);
    if user_data >= FENCE_NONCE_BASE && handle_fence(user_data, seq, event_type) {
        return event.as_ptr();
    }

    let input = match event_type {
        CGEventType::LeftMouseDown => Input::MouseDown(Button::Left),
        CGEventType::RightMouseDown => Input::MouseDown(Button::Right),
        CGEventType::OtherMouseDown => Input::MouseDown(Button::Other),
        CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
            let keycode = field(CGEventField::KeyboardEventKeycode);
            let autorepeat = field(CGEventField::KeyboardEventAutorepeat) != 0;
            let flags = CGEvent::flags(Some(event_ref));
            log_key(seq, event_type, keycode, autorepeat, flags);
            if event_type != CGEventType::KeyDown {
                return event.as_ptr();
            }
            if keycode == SPACE_KEYCODE {
                // The hotkey itself and its autorepeats never count as input.
                let wanted = CGEventFlags::MaskControl | CGEventFlags::MaskAlternate;
                if !autorepeat && flags.contains(wanted) {
                    let location = CGEvent::location(Some(event_ref));
                    let press = TapPress {
                        point: (location.x, location.y),
                        seen_at,
                        seq,
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
        // Marker types without a fence nonce, such as real scrolling.
        _ => return event.as_ptr(),
    };

    let delay = INPUT_DELAY_MS.load(Ordering::Relaxed);
    if delay > 0 {
        // Scenario F3: a slow tap thread. Events behind this one wait in the
        // tap's port meanwhile.
        thread::sleep(Duration::from_millis(delay));
    }
    LAST_INPUT_SEQ.store(seq, Ordering::Relaxed);
    let mut inputs = INPUTS.lock().unwrap_or_else(PoisonError::into_inner);
    if inputs.len() == MAX_INPUTS {
        inputs.pop_front();
    }
    inputs.push_back(TapInput {
        input,
        seen_at,
        seq,
    });
    event.as_ptr()
}

/// Records the fence `nonce` if it was posted by this process. Returns
/// whether the event was a marker.
fn handle_fence(nonce: i64, seq: u64, event_type: CGEventType) -> bool {
    let posted_at = {
        let mut pending = PENDING_FENCES
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(index) = pending.iter().position(|(posted, _)| *posted == nonce) else {
            return false;
        };
        pending.swap_remove(index).1
    };
    let snapshot = FenceSnapshot {
        seq,
        last_input_seq: LAST_INPUT_SEQ.load(Ordering::Relaxed),
        last_interrupt_seq: LAST_INTERRUPT_SEQ.load(Ordering::Relaxed),
        latency: posted_at.elapsed(),
    };
    info!(nonce, ?event_type, ?snapshot, "fence seen by tap");
    let mut fences = FENCES.lock().unwrap_or_else(PoisonError::into_inner);
    if fences.len() == MAX_FENCES {
        fences.pop_front();
    }
    fences.push_back((nonce, snapshot));
    FENCE_SEEN.notify_all();
    true
}

/// Scenario F1: what the tap sees of the hotkey keys and Esc.
fn log_key(seq: u64, event_type: CGEventType, keycode: i64, autorepeat: bool, flags: CGEventFlags) {
    let key = match keycode {
        SPACE_KEYCODE => "space",
        ESCAPE_KEYCODE => "escape",
        _ if event_type == CGEventType::FlagsChanged => "modifier",
        // Other keys are logged as guard inputs, not here.
        _ => return,
    };
    // One line per autorepeat would flood the log; the first of each press
    // is enough to see that the flag is set.
    static LOGGED_REPEAT: AtomicBool = AtomicBool::new(false);
    if autorepeat {
        if LOGGED_REPEAT.swap(true, Ordering::Relaxed) {
            return;
        }
    } else if event_type == CGEventType::KeyDown {
        LOGGED_REPEAT.store(false, Ordering::Relaxed);
    }
    info!(
        seq,
        ?event_type,
        key,
        keycode,
        autorepeat,
        control = flags.contains(CGEventFlags::MaskControl),
        option = flags.contains(CGEventFlags::MaskAlternate),
        shift = flags.contains(CGEventFlags::MaskShift),
        command = flags.contains(CGEventFlags::MaskCommand),
        "tap key"
    );
}
