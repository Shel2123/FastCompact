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
//! With `ORBIT_SPIKE_ACTIVE=1` the tap is active (stage 4 step 0b, F5): it
//! keeps its own hold state and swallows Esc while the hotkey is held, with
//! no help from the main thread. A timer on the tap's run loop logs Secure
//! Input and the HID key and button state (F6–F8).
//!
//! Needs Input Monitoring access; without it the tap is not created. An
//! active tap may need Accessibility instead (F5).

use std::cell::OnceCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFMachPort, CFRetained, CFRunLoop, CFRunLoopTimer,
    kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
    CGMouseButton, CGPreflightListenEventAccess, CGPreflightPostEventAccess,
    CGRequestListenEventAccess, CGScrollEventUnit,
};
use tracing::{error, info, warn};

/// Virtual key code of Space.
const SPACE_KEYCODE: i64 = 0x31;
/// Virtual key code of Esc.
const ESCAPE_KEYCODE: i64 = 0x35;
/// How often the timer on the tap's run loop reads the input state (F6–F8).
const POLL_INTERVAL: f64 = 0.05;
/// Space as a `CGKeyCode`, for the HID key state.
const SPACE_KEY: u16 = 0x31;

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
/// The tap may swallow events (F5).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The tap's own view of the hotkey hold, without the main thread.
static HELD: AtomicBool = AtomicBool::new(false);
/// The tap swallowed an Esc key-down and swallows its key-up too.
static ESCAPE_SWALLOWED: AtomicBool = AtomicBool::new(false);
/// Mouse buttons down as seen by the tap: bit 0 left, 1 right, 2 other.
static BUTTONS: AtomicU8 = AtomicU8::new(0);
/// A drag during the current hold was already logged.
static DRAG_LOGGED: AtomicBool = AtomicBool::new(false);
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

/// Whether Secure Event Input is on: the tap then gets no keyboard events.
pub fn secure_input_enabled() -> bool {
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn IsSecureEventInputEnabled() -> u8;
    }
    // SAFETY: takes no arguments and only reads a system flag.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// Starts the tap thread. `input_delay` makes the tap wait that long before
/// recording each mouse-down or key-down (scenario F3); `fence` adds the
/// marker's event type to the tap mask; `active` makes the tap able to
/// swallow Esc (F5).
pub fn spawn(input_delay: Duration, fence: Option<FenceKind>, active: bool) {
    if !CGPreflightListenEventAccess() {
        warn!(
            requested = CGRequestListenEventAccess(),
            "input monitoring not granted yet"
        );
    }
    let delay_ms = u64::try_from(input_delay.as_millis()).unwrap_or(u64::MAX);
    INPUT_DELAY_MS.store(delay_ms, Ordering::Relaxed);
    ACTIVE.store(active, Ordering::Relaxed);
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
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseUp,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
    ];
    if let Some(kind) = fence {
        types.push(kind.event_type());
    }
    let mask: CGEventMask = types
        .iter()
        .fold(0, |mask, event_type| mask | 1 << event_type.0);
    let active = ACTIVE.load(Ordering::Relaxed);
    let options = if active {
        CGEventTapOptions::Default
    } else {
        CGEventTapOptions::ListenOnly
    };
    // F5: which access an active tap needs.
    let listen = CGPreflightListenEventAccess();
    let post = CGPreflightPostEventAccess();
    // SAFETY: the callback matches CGEventTapCallBack and ignores user_info.
    let port: Option<CFRetained<CFMachPort>> = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            options,
            mask,
            Some(on_event),
            std::ptr::null_mut(),
        )
    };
    let Some(port) = port else {
        error!(active, listen, post, "event tap not created");
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
    // SAFETY: the callback matches CFRunLoopTimerCallBack and ignores its
    // info pointer, so no context is needed.
    let timer = unsafe {
        CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + POLL_INTERVAL,
            POLL_INTERVAL,
            0,
            0,
            Some(on_poll),
            std::ptr::null_mut(),
        )
    };
    match &timer {
        Some(timer) => run_loop.add_timer(Some(timer), modes),
        None => warn!("input state timer not created"),
    }
    CGEvent::tap_enable(&port, true);
    PORT.with(|cell| {
        if cell.set(port).is_err() {
            warn!("tap port was already stored");
        }
    });
    RUNNING.store(true, Ordering::Release);
    info!(
        ?fence,
        active,
        listen,
        post,
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
        CGEventType::LeftMouseDown => mouse_down(Button::Left),
        CGEventType::RightMouseDown => mouse_down(Button::Right),
        CGEventType::OtherMouseDown => mouse_down(Button::Other),
        CGEventType::LeftMouseUp => return mouse_up(event, seq, Button::Left),
        CGEventType::RightMouseUp => return mouse_up(event, seq, Button::Right),
        CGEventType::OtherMouseUp => return mouse_up(event, seq, Button::Other),
        CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            if HELD.load(Ordering::Relaxed) && !DRAG_LOGGED.swap(true, Ordering::Relaxed) {
                info!(seq, ?event_type, "tap drag during hold");
            }
            return event.as_ptr();
        }
        CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
            let keycode = field(CGEventField::KeyboardEventKeycode);
            let autorepeat = field(CGEventField::KeyboardEventAutorepeat) != 0;
            let flags = CGEvent::flags(Some(event_ref));
            log_key(seq, event_type, keycode, autorepeat, flags);
            let wanted = CGEventFlags::MaskControl | CGEventFlags::MaskAlternate;
            track_hold(seq, event_type, keycode, autorepeat, flags.contains(wanted));
            if keycode == ESCAPE_KEYCODE && swallow_escape(seq, event_type) {
                return std::ptr::null_mut();
            }
            if event_type != CGEventType::KeyDown {
                return event.as_ptr();
            }
            if keycode == SPACE_KEYCODE {
                // The hotkey itself and its autorepeats never count as input.
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

fn button_bit(button: Button) -> u8 {
    match button {
        Button::Left => 1,
        Button::Right => 2,
        Button::Other => 4,
    }
}

fn mouse_down(button: Button) -> Input {
    BUTTONS.fetch_or(button_bit(button), Ordering::Relaxed);
    // F6: whether mouse events still reach the tap under Secure Input.
    info!(?button, "tap mouse-down");
    Input::MouseDown(button)
}

fn mouse_up(event: NonNull<CGEvent>, seq: u64, button: Button) -> *mut CGEvent {
    BUTTONS.fetch_and(!button_bit(button), Ordering::Relaxed);
    if HELD.load(Ordering::Relaxed) {
        info!(seq, ?button, "tap mouse-up during hold");
    }
    event.as_ptr()
}

/// The tap's own hold state (F5): set by the hotkey key-down, cleared by
/// Space key-up or by letting go of Control or Option.
fn track_hold(seq: u64, event_type: CGEventType, keycode: i64, autorepeat: bool, mods: bool) {
    let pressed =
        event_type == CGEventType::KeyDown && keycode == SPACE_KEYCODE && !autorepeat && mods;
    let released = (event_type == CGEventType::KeyUp && keycode == SPACE_KEYCODE)
        || (event_type == CGEventType::FlagsChanged && !mods);
    if pressed {
        DRAG_LOGGED.store(false, Ordering::Relaxed);
        // F8: is a button held at the press, by the HID state and by the
        // tap's own count?
        let hid_left =
            CGEventSource::button_state(CGEventSourceStateID::HIDSystemState, CGMouseButton::Left);
        let was_held = HELD.swap(true, Ordering::Relaxed);
        info!(
            seq,
            was_held,
            hid_left,
            tap_buttons = BUTTONS.load(Ordering::Relaxed),
            "tap hold start"
        );
    } else if released && HELD.swap(false, Ordering::Relaxed) {
        info!(seq, ?event_type, "tap hold end");
    }
}

/// Swallows Esc while the hotkey is held, when the tap is active (F5).
/// Returns whether the event must be dropped.
fn swallow_escape(seq: u64, event_type: CGEventType) -> bool {
    if !ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    match event_type {
        CGEventType::KeyDown if HELD.load(Ordering::Relaxed) => {
            ESCAPE_SWALLOWED.store(true, Ordering::Relaxed);
            info!(seq, "tap swallowed escape key-down");
            true
        }
        CGEventType::KeyDown => {
            ESCAPE_SWALLOWED.store(false, Ordering::Relaxed);
            false
        }
        CGEventType::KeyUp if ESCAPE_SWALLOWED.swap(false, Ordering::Relaxed) => {
            info!(seq, "tap swallowed escape key-up");
            true
        }
        _ => false,
    }
}

/// Input state read on the tap's run loop (F6–F8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PollState {
    secure_input: bool,
    held: bool,
    hid_space: bool,
    hid_control: bool,
    hid_option: bool,
    hid_left: bool,
}

impl PollState {
    fn now() -> Self {
        let hid = CGEventSourceStateID::HIDSystemState;
        let flags = CGEventSource::flags_state(hid);
        Self {
            secure_input: secure_input_enabled(),
            held: HELD.load(Ordering::Relaxed),
            hid_space: CGEventSource::key_state(hid, SPACE_KEY),
            hid_control: flags.contains(CGEventFlags::MaskControl),
            hid_option: flags.contains(CGEventFlags::MaskAlternate),
            hid_left: CGEventSource::button_state(hid, CGMouseButton::Left),
        }
    }
}

/// Logs every change of the input state. Lets F7 compare what the tap saw
/// with the HID state while Secure Input hides the keyboard from the tap.
unsafe extern "C-unwind" fn on_poll(_timer: *mut CFRunLoopTimer, _info: *mut c_void) {
    static LAST: Mutex<Option<PollState>> = Mutex::new(None);
    let state = PollState::now();
    let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
    if *last != Some(state) {
        info!(?state, seq = SEQ.load(Ordering::Relaxed), "input state");
        *last = Some(state);
    }
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
