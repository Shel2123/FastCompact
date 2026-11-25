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
//! Step 0c (F7b–F11): the tap also checks every hold for continuity.
//! At the press and at the release it reads the HID keyboard counters and
//! posts a marker; when the marker comes back, the HID count between the two
//! markers must equal the number of key events the tap saw. It swallows every
//! Orbit marker, logs what the system reports about the tap itself, and
//! counts hotkey key-downs so that each Carbon press can be matched to one.
//!
//! A listen-only tap needs Input Monitoring or Accessibility; an active tap
//! needs Accessibility, Input Monitoring alone does not create it (F9). An
//! active tap that loses Accessibility stalls all input until the system's
//! timeout disables it, so the tap thread checks access on a timer and
//! removes the tap instead of enabling it again (F9e).

use std::cell::{OnceCell, RefCell};
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
    CGError, CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventSource, CGEventSourceStateID,
    CGEventTapInformation, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGGetEventTapList, CGMouseButton, CGPreflightListenEventAccess,
    CGPreflightPostEventAccess, CGRequestListenEventAccess, CGScrollEventUnit,
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
/// More system disables than this within [`DISABLE_WINDOW`] remove the tap
/// instead of enabling it again.
const MAX_DISABLES: usize = 3;
const DISABLE_WINDOW: Duration = Duration::from_secs(10);
/// How often the tap thread checks that the process still has access (F9e).
const ACCESS_CHECK_INTERVAL: f64 = 0.25;

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

/// How the tap thread runs.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// Delay before recording each mouse-down or key-down (F3).
    pub input_delay: Duration,
    /// Delay in the callback of each hotkey key-down (F10).
    pub hotkey_delay: Duration,
    /// Delay in the callback of each Orbit marker, so that a fence times out
    /// and its marker arrives late (F11).
    pub marker_delay: Duration,
    /// Adds the marker's event type to the tap mask (F2).
    pub fence: Option<FenceKind>,
    /// The tap may swallow events (F5, F11).
    pub active: bool,
    /// Run the input state timer (watchdog); off for F7b.
    pub poll: bool,
}

/// Keep at most this many recent inputs.
const MAX_INPUTS: usize = 64;
/// Keep at most this many handled fences.
const MAX_FENCES: usize = 16;
/// Upper 32 bits of the user data of every Orbit marker.
const MARKER_TAG: i64 = 0x0B17;
/// Marker nonces start here, so that the user data of other synthetic events
/// is not taken for a marker.
const FENCE_NONCE_BASE: i64 = MARKER_TAG << 32;

static LAST_PRESS: Mutex<Option<TapPress>> = Mutex::new(None);
static INPUTS: Mutex<VecDeque<TapInput>> = Mutex::new(VecDeque::new());
static RUNNING: AtomicBool = AtomicBool::new(false);
/// A thread is creating the tap now.
static STARTING: AtomicBool = AtomicBool::new(false);
static SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);
/// Mask the tap was created with.
static REQUIRED_MASK: AtomicU64 = AtomicU64::new(0);
/// Sequence number of the last event the tap handled.
static SEQ: AtomicU64 = AtomicU64::new(0);
static LAST_INPUT_SEQ: AtomicU64 = AtomicU64::new(0);
static LAST_INTERRUPT_SEQ: AtomicU64 = AtomicU64::new(0);
/// Delay the tap applies before recording an input (scenario F3).
static INPUT_DELAY_MS: AtomicU64 = AtomicU64::new(0);
/// Delay the tap applies to each hotkey key-down (F10).
static HOTKEY_DELAY_MS: AtomicU64 = AtomicU64::new(0);
/// Delay the tap applies to each Orbit marker (F11).
static MARKER_DELAY_MS: AtomicU64 = AtomicU64::new(0);
/// The tap may swallow events (F5).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The tap's own view of the hotkey hold, without the main thread.
static HELD: AtomicBool = AtomicBool::new(false);
/// Number of the current or last hold.
static HOLD: AtomicU64 = AtomicU64::new(0);
/// The watchdog already reported the current hold.
static WATCHDOG_FIRED: AtomicBool = AtomicBool::new(false);
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
/// Key events that passed through the callback, swallowed ones included.
static KEY_DOWNS: AtomicU64 = AtomicU64::new(0);
static KEY_REPEATS: AtomicU64 = AtomicU64::new(0);
static KEY_UPS: AtomicU64 = AtomicU64::new(0);
/// Events seen per `CGEventType` value (F9).
static SEEN_BY_TYPE: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];
/// Hotkey key-downs whose callback started and finished (F10).
static HOTKEY_STARTED: AtomicU64 = AtomicU64::new(0);
static HOTKEY_DOWNS: AtomicU64 = AtomicU64::new(0);
/// Hotkey key-downs already matched to a Carbon press; main thread only.
static HOTKEY_MATCHED: AtomicU64 = AtomicU64::new(0);
static CONTINUITY: Mutex<Continuity> = Mutex::new(Continuity::new());

thread_local! {
    /// The tap's port, to enable it again after the system disables it.
    static PORT: OnceCell<CFRetained<CFMachPort>> = const { OnceCell::new() };
    /// When the system disabled the tap, newest last.
    static DISABLES: RefCell<VecDeque<Instant>> = const { RefCell::new(VecDeque::new()) };
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

/// Whether `user_data` belongs to an Orbit marker.
pub fn is_orbit_marker(user_data: i64) -> bool {
    user_data >> 32 == MARKER_TAG
}

fn next_nonce() -> i64 {
    NEXT_NONCE.fetch_add(1, Ordering::Relaxed)
}

fn marker_event(kind: FenceKind) -> Option<CFRetained<CGEvent>> {
    match kind {
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
    }
}

fn post_marker(kind: FenceKind, nonce: i64) -> Result<(), String> {
    let Some(event) = marker_event(kind) else {
        return Err(format!("{kind:?} marker event not created"));
    };
    CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, nonce);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    Ok(())
}

/// Posts a fence marker and returns its nonce. The nonce is registered
/// before the marker is posted, so an early acknowledgement is kept.
pub fn post_fence(kind: FenceKind) -> Result<i64, String> {
    let nonce = next_nonce();
    PENDING_FENCES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((nonce, Instant::now()));
    if let Err(err) = post_marker(kind, nonce) {
        forget_fence(nonce);
        return Err(err);
    }
    Ok(nonce)
}

fn forget_fence(nonce: i64) {
    PENDING_FENCES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|(pending, _)| *pending != nonce);
}

/// Waits until the tap handles the fence `nonce`, at most `timeout`. On
/// timeout the nonce is forgotten: a marker arriving later is logged as late
/// (F11).
pub fn wait_fence(nonce: i64, timeout: Duration) -> Option<FenceSnapshot> {
    let deadline = Instant::now() + timeout;
    let mut fences = FENCES.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if let Some((_, snapshot)) = fences.iter().find(|(seen, _)| *seen == nonce) {
            return Some(*snapshot);
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            drop(fences);
            forget_fence(nonce);
            return None;
        };
        fences = FENCE_SEEN
            .wait_timeout(fences, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

/// Whether Secure Event Input is on: the tap then gets no keyboard events.
pub fn secure_input_enabled() -> bool {
    // SAFETY: takes no arguments and only reads a system flag.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// Starts the tap thread.
pub fn spawn(settings: Settings) {
    if !CGPreflightListenEventAccess() {
        warn!(
            requested = CGRequestListenEventAccess(),
            "input monitoring not granted yet"
        );
    }
    let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    INPUT_DELAY_MS.store(millis(settings.input_delay), Ordering::Relaxed);
    HOTKEY_DELAY_MS.store(millis(settings.hotkey_delay), Ordering::Relaxed);
    MARKER_DELAY_MS.store(millis(settings.marker_delay), Ordering::Relaxed);
    ACTIVE.store(settings.active, Ordering::Relaxed);
    *SETTINGS.lock().unwrap_or_else(PoisonError::into_inner) = Some(settings);
    start_thread(settings);
}

/// Creates the tap again if it is not running, such as after access was
/// granted (F9).
pub fn ensure_running() {
    if running() {
        return;
    }
    let settings = *SETTINGS.lock().unwrap_or_else(PoisonError::into_inner);
    match settings {
        Some(settings) => {
            info!("event tap not running; trying to create it again");
            start_thread(settings);
        }
        None => warn!("event tap was never configured"),
    }
}

fn start_thread(settings: Settings) {
    if STARTING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = thread::Builder::new()
        .name("orbit-spike-tap".into())
        .spawn(move || run_tap(settings));
    if let Err(err) = spawned {
        STARTING.store(false, Ordering::Release);
        error!(%err, "event tap thread not started");
    }
}

fn run_tap(settings: Settings) {
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
        // Continuity markers (F7b) and the scroll fence.
        CGEventType::ScrollWheel,
    ];
    if let Some(kind) = settings.fence {
        types.push(kind.event_type());
    }
    let mask: CGEventMask = types
        .iter()
        .fold(0, |mask, event_type| mask | 1 << event_type.0);
    REQUIRED_MASK.store(mask, Ordering::Relaxed);
    let options = if settings.active {
        CGEventTapOptions::Default
    } else {
        CGEventTapOptions::ListenOnly
    };
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
        STARTING.store(false, Ordering::Release);
        error!(status = ?status(), "event tap not created");
        return;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        STARTING.store(false, Ordering::Release);
        error!("event tap run loop source not created");
        return;
    };
    let Some(run_loop) = CFRunLoop::current() else {
        STARTING.store(false, Ordering::Release);
        error!("no run loop for the tap thread");
        return;
    };
    // SAFETY: reading an immutable framework constant.
    let modes = unsafe { kCFRunLoopCommonModes };
    run_loop.add_source(Some(&source), modes);
    if settings.poll {
        // SAFETY: the callback matches CFRunLoopTimerCallBack and ignores
        // its info pointer, so no context is needed.
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
    }
    // SAFETY: as for the input state timer.
    let access_timer = unsafe {
        CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + ACCESS_CHECK_INTERVAL,
            ACCESS_CHECK_INTERVAL,
            0,
            0,
            Some(on_access_check),
            std::ptr::null_mut(),
        )
    };
    match &access_timer {
        Some(timer) => run_loop.add_timer(Some(timer), modes),
        None => warn!("access check timer not created"),
    }
    CGEvent::tap_enable(&port, true);
    PORT.with(|cell| {
        if cell.set(port).is_err() {
            warn!("tap port was already stored");
        }
    });
    RUNNING.store(true, Ordering::Release);
    STARTING.store(false, Ordering::Release);
    info!(
        ?settings,
        mask = format_args!("{mask:#x}"),
        status = ?status(),
        "event tap running"
    );
    CFRunLoop::run();
    info!("event tap removed; tap thread ends");
}

/// What the system reports about the tap and the access it depends on (F9).
#[derive(Debug, Clone, Copy)]
#[expect(dead_code, reason = "fields are only logged through Debug")]
pub struct Status {
    running: bool,
    /// From `CGGetEventTapList`: `(enabled, eventsOfInterest, missing)`,
    /// where `missing` are the required types the tap does not get.
    listed: Option<(bool, u64, u64)>,
    listen_access: bool,
    post_access: bool,
    ax_trusted: bool,
    secure_input: bool,
}

impl Status {
    /// The tap gets every required event type.
    pub fn ready(&self) -> bool {
        self.running && matches!(self.listed, Some((true, _, 0)))
    }

    /// Whether the process has the access its tap needs (F9): an active tap
    /// needs Accessibility, Input Monitoring alone does not create it; a
    /// listen-only tap works with either.
    pub fn has_access(&self) -> bool {
        if ACTIVE.load(Ordering::Relaxed) {
            self.ax_trusted
        } else {
            self.ax_trusted || self.listen_access
        }
    }

    /// The refusal the product would give for a Carbon press the tap did
    /// not see (stage 4 plan, `carbon_pressed`).
    pub fn refusal(&self) -> &'static str {
        if self.secure_input {
            "SecureInputOn"
        } else if !self.ax_trusted {
            "AccessibilityDenied"
        } else if !self.listen_access {
            "InputMonitoringDenied"
        } else if !self.ready() {
            "Failed(event tap is not receiving keyboard events)"
        } else {
            "Failed(hotkey was not seen by the event tap)"
        }
    }
}

pub fn status() -> Status {
    let required = REQUIRED_MASK.load(Ordering::Relaxed);
    let listed = own_tap().map(|info| {
        (
            info.enabled,
            info.eventsOfInterest,
            required & !info.eventsOfInterest,
        )
    });
    Status {
        running: running(),
        listed,
        listen_access: CGPreflightListenEventAccess(),
        post_access: CGPreflightPostEventAccess(),
        // SAFETY: takes no arguments and only reads the trust state.
        ax_trusted: unsafe { objc2_application_services::AXIsProcessTrusted() },
        secure_input: secure_input_enabled(),
    }
}

/// The tap this process created, as listed by the system.
fn own_tap() -> Option<CGEventTapInformation> {
    let mut count: u32 = 0;
    // SAFETY: a null list with a count pointer asks only for the number of
    // taps.
    let err = unsafe { CGGetEventTapList(0, std::ptr::null_mut(), &raw mut count) };
    if err != CGError::Success || count == 0 {
        return None;
    }
    let capacity = usize::try_from(count).ok()?;
    let mut list: Vec<CGEventTapInformation> = Vec::with_capacity(capacity);
    let mut filled: u32 = 0;
    // SAFETY: the list has room for `count` entries and the system writes at
    // most that many.
    let err = unsafe { CGGetEventTapList(count, list.as_mut_ptr(), &raw mut filled) };
    if err != CGError::Success {
        return None;
    }
    let filled = usize::try_from(filled).ok()?.min(capacity);
    // SAFETY: the system initialised the first `filled` entries.
    unsafe { list.set_len(filled) };
    let own = i32::try_from(std::process::id()).ok()?;
    list.into_iter().find(|info| info.tappingProcess == own)
}

/// Event counts by type name, for the types the tap asked for (F9).
pub fn seen_by_type() -> Vec<(&'static str, u64)> {
    [
        ("key_down", CGEventType::KeyDown),
        ("key_up", CGEventType::KeyUp),
        ("flags", CGEventType::FlagsChanged),
        ("left_down", CGEventType::LeftMouseDown),
        ("left_up", CGEventType::LeftMouseUp),
        ("left_drag", CGEventType::LeftMouseDragged),
        ("scroll", CGEventType::ScrollWheel),
    ]
    .into_iter()
    .map(|(name, event_type)| (name, seen_count(event_type)))
    .collect()
}

fn seen_count(event_type: CGEventType) -> u64 {
    usize::try_from(event_type.0)
        .ok()
        .and_then(|index| SEEN_BY_TYPE.get(index))
        .map_or(0, |count| count.load(Ordering::Relaxed))
}

/// Result of matching a Carbon press to a tap key-down (F10).
#[derive(Debug, Clone, Copy)]
#[expect(dead_code, reason = "fields are only logged through Debug")]
pub struct HotkeyMatch {
    /// The tap saw a hotkey key-down that no earlier Carbon press took.
    pub matched: bool,
    /// A hotkey callback had started but not finished when Carbon reported.
    pub in_flight: bool,
    started: u64,
    finished: u64,
    matched_total: u64,
}

/// Matches one Carbon press to one tap key-down. Main thread only.
pub fn match_hotkey() -> HotkeyMatch {
    let finished = HOTKEY_DOWNS.load(Ordering::Acquire);
    let started = HOTKEY_STARTED.load(Ordering::Acquire);
    let taken = HOTKEY_MATCHED.load(Ordering::Relaxed);
    let matched = finished > taken;
    let matched_total = if matched { taken + 1 } else { taken };
    HOTKEY_MATCHED.store(matched_total, Ordering::Relaxed);
    HotkeyMatch {
        matched,
        in_flight: started > finished,
        started,
        finished,
        matched_total,
    }
}

/// Records a system disable at `at`; whether there were more than
/// [`MAX_DISABLES`] within [`DISABLE_WINDOW`]. Tap thread only.
fn note_disable(at: Instant) -> bool {
    DISABLES.with_borrow_mut(|disables| {
        disables.push_back(at);
        while disables
            .front()
            .is_some_and(|first| at.duration_since(*first) > DISABLE_WINDOW)
        {
            disables.pop_front();
        }
        disables.len() > MAX_DISABLES
    })
}

/// Invalidates the tap and stops its run loop, which ends the tap thread.
/// Tap thread only.
fn remove_tap() {
    PORT.with(|cell| {
        if let Some(port) = cell.get() {
            port.invalidate();
        }
    });
    RUNNING.store(false, Ordering::Release);
    match CFRunLoop::current() {
        Some(run_loop) => run_loop.stop(),
        None => error!("no run loop for the tap thread; it keeps running"),
    }
}

unsafe extern "C-unwind" fn on_event(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    let seen_at = Instant::now();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    if let Some(count) = usize::try_from(event_type.0)
        .ok()
        .and_then(|index| SEEN_BY_TYPE.get(index))
    {
        count.fetch_add(1, Ordering::Relaxed);
    }
    if event_type == CGEventType::TapDisabledByTimeout
        || event_type == CGEventType::TapDisabledByUserInput
    {
        LAST_INTERRUPT_SEQ.store(seq, Ordering::Relaxed);
        let status = status();
        let storm = note_disable(seen_at);
        // F9e: an active tap whose access was revoked stays in the event
        // path; enabling it again makes every event wait for the timeout and
        // stalls all input. Remove it instead; `ensure_running` creates it
        // again once access is back.
        if !status.has_access() || storm {
            warn!(
                seq,
                ?event_type,
                ?status,
                storm,
                "event tap disabled; removing it"
            );
            remove_tap();
        } else {
            warn!(
                seq,
                ?event_type,
                ?status,
                "event tap disabled; enabling it again"
            );
            PORT.with(|cell| match cell.get() {
                Some(port) => CGEvent::tap_enable(port, true),
                None => error!("tap port unknown; the tap stays disabled"),
            });
        }
        return event.as_ptr();
    }
    // SAFETY: the system passes a valid event for the duration of the call.
    let event_ref = unsafe { event.as_ref() };
    let field = |field| CGEvent::integer_value_field(Some(event_ref), field);

    let user_data = field(CGEventField::EventSourceUserData);
    if is_orbit_marker(user_data) {
        let delay = MARKER_DELAY_MS.load(Ordering::Relaxed);
        if delay > 0 {
            thread::sleep(Duration::from_millis(delay));
        }
        let known = handle_fence(user_data, seq, event_type) || handle_continuity(user_data, seq);
        if !known {
            warn!(user_data, seq, ?event_type, "late or unknown orbit marker");
        }
        if ACTIVE.load(Ordering::Relaxed) {
            // F11: no Orbit marker reaches an application.
            return std::ptr::null_mut();
        }
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
            count_key(event_type, autorepeat);
            log_key(seq, event_type, keycode, autorepeat, flags);
            let wanted = CGEventFlags::MaskControl | CGEventFlags::MaskAlternate;
            let mods = flags.contains(wanted);
            let hotkey_down = event_type == CGEventType::KeyDown
                && keycode == SPACE_KEYCODE
                && !autorepeat
                && mods;
            if hotkey_down {
                HOTKEY_STARTED.fetch_add(1, Ordering::AcqRel);
            }
            track_hold(seq, event_type, keycode, autorepeat, mods);
            if keycode == ESCAPE_KEYCODE && swallow_escape(seq, event_type) {
                return std::ptr::null_mut();
            }
            if hotkey_down {
                let location = CGEvent::location(Some(event_ref));
                let press = TapPress {
                    point: (location.x, location.y),
                    seen_at,
                    seq,
                };
                *LAST_PRESS.lock().unwrap_or_else(PoisonError::into_inner) = Some(press);
                let delay = HOTKEY_DELAY_MS.load(Ordering::Relaxed);
                if delay > 0 {
                    // F10: does Carbon wait for the tap callback?
                    thread::sleep(Duration::from_millis(delay));
                }
                HOTKEY_DOWNS.fetch_add(1, Ordering::AcqRel);
                return event.as_ptr();
            }
            if event_type != CGEventType::KeyDown || keycode == SPACE_KEYCODE {
                // Space and its autorepeats never count as input.
                return event.as_ptr();
            }
            Input::KeyDown {
                keycode,
                autorepeat,
            }
        }
        // Marker types without an Orbit tag, such as real scrolling.
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

fn count_key(event_type: CGEventType, autorepeat: bool) {
    let counter = match event_type {
        CGEventType::KeyDown if autorepeat => &KEY_REPEATS,
        CGEventType::KeyDown => &KEY_DOWNS,
        CGEventType::KeyUp => &KEY_UPS,
        _ => return,
    };
    counter.fetch_add(1, Ordering::Relaxed);
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
        WATCHDOG_FIRED.store(false, Ordering::Relaxed);
        // F8: is a button held at the press, by the HID state and by the
        // tap's own count?
        let hid_left =
            CGEventSource::button_state(CGEventSourceStateID::HIDSystemState, CGMouseButton::Left);
        let was_held = HELD.swap(true, Ordering::Relaxed);
        let hold = HOLD.fetch_add(1, Ordering::Relaxed) + 1;
        info!(
            seq,
            hold,
            was_held,
            hid_left,
            tap_buttons = BUTTONS.load(Ordering::Relaxed),
            "tap hold start"
        );
        continuity_start(hold, seq);
    } else if released && HELD.swap(false, Ordering::Relaxed) {
        let cause = if event_type == CGEventType::KeyUp {
            "space"
        } else {
            "modifier"
        };
        info!(seq, ?event_type, cause, "tap hold end");
        continuity_release(HOLD.load(Ordering::Relaxed), seq, cause);
    }
}

/// HID keyboard counters, before Secure Input filtering (F7b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HidKeys {
    down: u32,
    up: u32,
}

impl HidKeys {
    fn now() -> Self {
        let hid = CGEventSourceStateID::HIDSystemState;
        Self {
            down: CGEventSource::counter_for_event_type(hid, CGEventType::KeyDown),
            up: CGEventSource::counter_for_event_type(hid, CGEventType::KeyUp),
        }
    }
}

/// Key events the tap saw, swallowed ones included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TapKeys {
    down: u64,
    repeat: u64,
    up: u64,
}

impl TapKeys {
    fn now() -> Self {
        Self {
            down: KEY_DOWNS.load(Ordering::Relaxed),
            repeat: KEY_REPEATS.load(Ordering::Relaxed),
            up: KEY_UPS.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerRole {
    Baseline,
    Release,
}

/// Continuity check of one hold (F7b/F7c).
#[derive(Debug)]
struct Continuity {
    hold: u64,
    baseline_hid: Option<HidKeys>,
    baseline_tap: Option<TapKeys>,
    release_hid: Option<HidKeys>,
    release_cause: &'static str,
    /// Markers posted for the hold: nonce, role, post time.
    markers: Vec<(i64, MarkerRole, Instant)>,
}

impl Continuity {
    const fn new() -> Self {
        Self {
            hold: 0,
            baseline_hid: None,
            baseline_tap: None,
            release_hid: None,
            release_cause: "",
            markers: Vec::new(),
        }
    }
}

fn continuity_start(hold: u64, seq: u64) {
    let hid = HidKeys::now();
    let nonce = next_nonce();
    {
        let mut state = CONTINUITY.lock().unwrap_or_else(PoisonError::into_inner);
        *state = Continuity::new();
        state.hold = hold;
        state.baseline_hid = Some(hid);
        state
            .markers
            .push((nonce, MarkerRole::Baseline, Instant::now()));
    }
    match post_marker(FenceKind::Scroll, nonce) {
        Ok(()) => info!(hold, seq, nonce, ?hid, "continuity baseline posted"),
        Err(err) => warn!(hold, %err, "continuity baseline not posted"),
    }
}

fn continuity_release(hold: u64, seq: u64, cause: &'static str) {
    let hid = HidKeys::now();
    let nonce = next_nonce();
    {
        let mut state = CONTINUITY.lock().unwrap_or_else(PoisonError::into_inner);
        if state.hold != hold || state.release_hid.is_some() {
            return;
        }
        state.release_hid = Some(hid);
        state.release_cause = cause;
        state
            .markers
            .push((nonce, MarkerRole::Release, Instant::now()));
    }
    match post_marker(FenceKind::Scroll, nonce) {
        Ok(()) => info!(hold, seq, nonce, ?hid, cause, "continuity release posted"),
        Err(err) => warn!(hold, %err, "continuity release not posted"),
    }
}

/// Handles a continuity marker. Returns whether `nonce` was one.
fn handle_continuity(nonce: i64, seq: u64) -> bool {
    let mut state = CONTINUITY.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(index) = state
        .markers
        .iter()
        .position(|(posted, ..)| *posted == nonce)
    else {
        return false;
    };
    let (_, role, posted_at) = state.markers.swap_remove(index);
    let tap = TapKeys::now();
    let latency = posted_at.elapsed();
    match role {
        MarkerRole::Baseline => {
            state.baseline_tap = Some(tap);
            info!(
                hold = state.hold,
                seq,
                ?tap,
                ?latency,
                "continuity baseline seen"
            );
        }
        MarkerRole::Release => {
            let (Some(b_hid), Some(b_tap), Some(r_hid)) =
                (state.baseline_hid, state.baseline_tap, state.release_hid)
            else {
                warn!(
                    hold = state.hold,
                    ?state,
                    "continuity release without baseline"
                );
                return true;
            };
            let hid_down = u64::from(r_hid.down.wrapping_sub(b_hid.down));
            let hid_up = u64::from(r_hid.up.wrapping_sub(b_hid.up));
            let tap_down = tap.down - b_tap.down;
            let tap_repeat = tap.repeat - b_tap.repeat;
            let tap_up = tap.up - b_tap.up;
            let hid_total = hid_down + hid_up;
            let with_repeats = hid_total == tap_down + tap_repeat + tap_up;
            let without_repeats = hid_total == tap_down + tap_up;
            info!(
                hold = state.hold,
                seq,
                cause = state.release_cause,
                hid_down,
                hid_up,
                tap_down,
                tap_repeat,
                tap_up,
                with_repeats,
                without_repeats,
                ?latency,
                verdict = if with_repeats || without_repeats {
                    "continuous"
                } else {
                    "MISMATCH"
                },
                "continuity"
            );
        }
    }
    true
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
/// Also logs when the planned watchdog would cancel the hold.
/// Removes the tap as soon as the process loses access (F9e), before the
/// system's timeout disables it and stalls input meanwhile.
unsafe extern "C-unwind" fn on_access_check(_timer: *mut CFRunLoopTimer, _info: *mut c_void) {
    let started = Instant::now();
    let status = status();
    let took = started.elapsed();
    if took > Duration::from_millis(5) {
        warn!(?took, "access check was slow");
    }
    if !status.has_access() {
        warn!(?status, ?took, "access lost; removing the event tap");
        remove_tap();
    }
}

unsafe extern "C-unwind" fn on_poll(_timer: *mut CFRunLoopTimer, _info: *mut c_void) {
    static LAST: Mutex<Option<PollState>> = Mutex::new(None);
    let state = PollState::now();
    if state.held
        && (state.secure_input || !state.hid_control || !state.hid_option)
        && !WATCHDOG_FIRED.swap(true, Ordering::Relaxed)
    {
        info!(
            hold = HOLD.load(Ordering::Relaxed),
            ?state,
            "watchdog: observation lost"
        );
    }
    let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
    if *last != Some(state) {
        info!(?state, seq = SEQ.load(Ordering::Relaxed), "input state");
        *last = Some(state);
    }
}

/// Records the fence `nonce` if it was posted by this process and is still
/// awaited. Returns whether the event was such a marker.
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
