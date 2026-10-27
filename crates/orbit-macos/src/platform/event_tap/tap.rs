//! The active event tap on its own thread.
//!
//! The tap runs on the `orbit-event-tap` thread with its own run loop. Its
//! callback converts each event, runs it through the [`GestureTracker`] and
//! swallows it or lets it pass in the same call; signals go to the handler
//! given to [`EventTap::start`], in order.
//!
//! An active tap needs Accessibility. When the process loses it, the tap
//! stays in the event path and the system stalls all input until a timeout
//! disables the tap; enabling it again repeats the stall (spike F9e). So the
//! `orbit-access` thread checks Accessibility on a timer, off the tap thread
//! because the check can take tens of milliseconds, and stops the tap as
//! soon as access is lost. The tap thread then removes the tap and ends the
//! gesture. When access comes back, the same thread creates the tap again.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFMachPort, CFRetained, CFRunLoop, CFRunLoopTimer,
    kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGError, CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventSource, CGEventSourceStateID,
    CGEventTapInformation, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGGetEventTapList, CGMouseButton,
};
use thiserror::Error;
use tracing::{error, info, warn};

use super::fence::post_marker;
use super::log::InputLog;
use super::tracker::{
    Button, Disposition, GestureTracker, HidKeys, Modifiers, PollState, QuartzPoint, Signal,
    TapEvent, TapIo,
};
use crate::platform::permissions::{accessibility_trusted, secure_input_enabled};

/// Event types the tap must get for the gesture and the guard.
const REQUIRED_TYPES: [CGEventType; 14] = [
    CGEventType::KeyDown,
    CGEventType::KeyUp,
    CGEventType::FlagsChanged,
    CGEventType::LeftMouseDown,
    CGEventType::RightMouseDown,
    CGEventType::OtherMouseDown,
    CGEventType::LeftMouseUp,
    CGEventType::RightMouseUp,
    CGEventType::OtherMouseUp,
    CGEventType::MouseMoved,
    CGEventType::LeftMouseDragged,
    CGEventType::RightMouseDragged,
    CGEventType::OtherMouseDragged,
    // Markers.
    CGEventType::ScrollWheel,
];

/// [`REQUIRED_TYPES`] as an event mask.
pub const REQUIRED_MASK: CGEventMask = {
    let mut mask = 0;
    let mut index = 0;
    while index < REQUIRED_TYPES.len() {
        mask |= 1 << REQUIRED_TYPES[index].0;
        index += 1;
    }
    mask
};

/// Period of the watchdog while a gesture is in progress, in seconds.
const POLL_INTERVAL: f64 = 0.05;

/// How far ahead the idle watchdog is parked, in seconds.
const PARKED: f64 = 365.0 * 24.0 * 3600.0;

/// Period of the Accessibility check.
const ACCESS_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// More system disables than this within [`DISABLE_WINDOW`] remove the tap
/// instead of enabling it again.
const MAX_DISABLES: usize = 3;
const DISABLE_WINDOW: Duration = Duration::from_secs(10);

/// Whether a tap was started in this process; there is one at most.
static STARTED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// State of the tap thread, set while its run loop runs.
    static TAP: RefCell<Option<TapThread>> = const { RefCell::new(None) };
}

/// Why the tap could not be started.
#[derive(Debug, Error)]
pub enum TapError {
    /// [`EventTap::start`] was called before.
    #[error("the event tap is already started")]
    AlreadyStarted,
    /// The access thread could not be started.
    #[error("access thread not started: {0}")]
    Thread(#[from] io::Error),
}

/// The tap as the system lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListedTap {
    /// The tap is enabled.
    pub enabled: bool,
    /// Event types the tap gets; the system may grant fewer than asked.
    pub events_of_interest: CGEventMask,
}

impl ListedTap {
    /// Enabled and getting every required event type.
    pub const fn receives_everything(&self) -> bool {
        self.enabled && REQUIRED_MASK & !self.events_of_interest == 0
    }
}

/// What the system reports about the tap and the access it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapStatus {
    /// The tap thread runs its tap.
    pub running: bool,
    /// The tap of this process in the system's list, if listed.
    pub listed: Option<ListedTap>,
    /// Accessibility is granted.
    pub accessibility: bool,
    /// Secure keyboard entry is on.
    pub secure_input: bool,
}

impl TapStatus {
    /// The tap runs, gets every required event type and keeps its access
    /// (spike F9: Accessibility alone is enough; the preflight checks of
    /// Input Monitoring report `false` even then and are not used).
    pub const fn ready(&self) -> bool {
        self.running
            && self.accessibility
            && match self.listed {
                Some(tap) => tap.receives_everything(),
                None => false,
            }
    }
}

/// Run loop of the tap thread, held by other threads only to stop it.
struct TapRunLoop(CFRetained<CFRunLoop>);

// SAFETY: Core Foundation run loops are thread-safe, and other threads only
// call `CFRunLoopStop` on this one, which is meant to be called from any
// thread.
unsafe impl Send for TapRunLoop {}
// SAFETY: as for `Send`.
unsafe impl Sync for TapRunLoop {}

struct Shared {
    log: Arc<InputLog>,
    on_signal: Box<dyn Fn(Signal) + Send + Sync>,
    /// The tap thread runs its run loop with the tap.
    running: AtomicBool,
    /// A tap thread was spawned and has not started running yet.
    starting: AtomicBool,
    /// Last result of the Accessibility check, for the tap thread.
    trusted: AtomicBool,
    run_loop: Mutex<Option<TapRunLoop>>,
}

impl Shared {
    fn run_loop(&self) -> MutexGuard<'_, Option<TapRunLoop>> {
        self.run_loop.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks the tap thread to remove the tap.
    fn stop_tap(&self) {
        if let Some(run_loop) = self.run_loop().as_ref() {
            run_loop.0.stop();
        }
    }
}

/// Handle of the event tap. Clones share the tap.
#[derive(Clone)]
pub struct EventTap {
    shared: Arc<Shared>,
}

impl fmt::Debug for EventTap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventTap")
            .field("running", &self.shared.running.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl EventTap {
    /// Starts watching Accessibility and creates the tap as soon as it is
    /// granted. `on_signal` gets the gesture signals on the tap thread, in
    /// order, and must not block.
    pub fn start(
        log: Arc<InputLog>,
        on_signal: impl Fn(Signal) + Send + Sync + 'static,
    ) -> Result<Self, TapError> {
        if STARTED.swap(true, Ordering::AcqRel) {
            return Err(TapError::AlreadyStarted);
        }
        let shared = Arc::new(Shared {
            log,
            on_signal: Box::new(on_signal),
            running: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            trusted: AtomicBool::new(false),
            run_loop: Mutex::new(None),
        });
        let weak = Arc::downgrade(&shared);
        let spawned = thread::Builder::new()
            .name("orbit-access".into())
            .spawn(move || watch_access(&weak));
        if let Err(err) = spawned {
            STARTED.store(false, Ordering::Release);
            return Err(err.into());
        }
        Ok(Self { shared })
    }

    /// What the system reports about the tap now. Takes up to tens of
    /// milliseconds.
    pub fn status(&self) -> TapStatus {
        TapStatus {
            running: self.shared.running.load(Ordering::Acquire),
            listed: own_tap(),
            accessibility: accessibility_trusted(false),
            secure_input: secure_input_enabled(),
        }
    }

    /// Creates the tap again if it is not running, such as after it was
    /// removed for too many disables.
    pub fn ensure_running(&self) {
        spawn_tap(&self.shared);
    }
}

/// Checks Accessibility until every handle and the tap thread are gone.
fn watch_access(weak: &Weak<Shared>) {
    let mut was_trusted = false;
    while let Some(shared) = weak.upgrade() {
        let trusted = accessibility_trusted(false);
        shared.trusted.store(trusted, Ordering::Release);
        if trusted && !was_trusted {
            info!("accessibility granted; creating the event tap");
            spawn_tap(&shared);
        } else if !trusted && shared.running.load(Ordering::Acquire) {
            warn!("accessibility lost; removing the event tap");
            shared.stop_tap();
        }
        was_trusted = trusted;
        drop(shared);
        thread::sleep(ACCESS_CHECK_INTERVAL);
    }
}

fn spawn_tap(shared: &Arc<Shared>) {
    if shared.running.load(Ordering::Acquire) || shared.starting.swap(true, Ordering::AcqRel) {
        return;
    }
    let tap_shared = Arc::clone(shared);
    let spawned = thread::Builder::new()
        .name("orbit-event-tap".into())
        .spawn(move || run_tap(tap_shared));
    if let Err(err) = spawned {
        shared.starting.store(false, Ordering::Release);
        error!(%err, "event tap thread not started");
    }
}

/// State of the tap thread.
struct TapThread {
    shared: Arc<Shared>,
    tracker: GestureTracker,
    port: CFRetained<CFMachPort>,
    watchdog: Option<CFRetained<CFRunLoopTimer>>,
    watching: bool,
    disables: VecDeque<Instant>,
}

fn run_tap(shared: Arc<Shared>) {
    let Some(thread) = create_tap(&shared) else {
        shared.starting.store(false, Ordering::Release);
        return;
    };
    let Some(run_loop) = CFRunLoop::current() else {
        shared.starting.store(false, Ordering::Release);
        thread.port.invalidate();
        error!("no run loop for the tap thread");
        return;
    };
    *shared.run_loop() = Some(TapRunLoop(run_loop));
    CGEvent::tap_enable(&thread.port, true);
    TAP.set(Some(thread));
    shared.running.store(true, Ordering::Release);
    shared.starting.store(false, Ordering::Release);
    info!("event tap running");

    CFRunLoop::run();

    *shared.run_loop() = None;
    shared.running.store(false, Ordering::Release);
    let Some(mut thread) = TAP.take() else {
        return;
    };
    thread.port.invalidate();
    if let Some(timer) = &thread.watchdog {
        timer.invalidate();
    }
    let out = thread
        .tracker
        .on_event(TapEvent::TapDisabled, Instant::now(), &mut SystemIo);
    thread.emit(out.signals);
    info!("event tap removed");
}

/// Creates the tap and adds it and the watchdog to the run loop of the
/// current thread.
fn create_tap(shared: &Arc<Shared>) -> Option<TapThread> {
    // SAFETY: the callback matches CGEventTapCallBack and ignores user_info.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            REQUIRED_MASK,
            Some(on_event),
            std::ptr::null_mut(),
        )
    };
    let Some(port) = port else {
        warn!("event tap not created; Accessibility is probably missing");
        return None;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        port.invalidate();
        error!("event tap run loop source not created");
        return None;
    };
    let run_loop = CFRunLoop::current()?;
    // SAFETY: reading an immutable framework constant.
    let modes = unsafe { kCFRunLoopCommonModes };
    run_loop.add_source(Some(&source), modes);
    // SAFETY: the callback matches CFRunLoopTimerCallBack and ignores its
    // info pointer, so no context is needed.
    let watchdog = unsafe {
        CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + PARKED,
            POLL_INTERVAL,
            0,
            0,
            Some(on_watchdog),
            std::ptr::null_mut(),
        )
    };
    match &watchdog {
        Some(timer) => run_loop.add_timer(Some(timer), modes),
        None => error!("watchdog timer not created; gestures are cancelled"),
    }
    Some(TapThread {
        shared: Arc::clone(shared),
        tracker: GestureTracker::new(Arc::clone(&shared.log)),
        port,
        watchdog,
        watching: false,
        disables: VecDeque::new(),
    })
}

impl TapThread {
    fn handle(&mut self, event_type: CGEventType, event: NonNull<CGEvent>) -> Disposition {
        let now = Instant::now();
        if event_type == CGEventType::TapDisabledByTimeout
            || event_type == CGEventType::TapDisabledByUserInput
        {
            self.disabled(event_type, now);
            return Disposition::Pass;
        }
        // SAFETY: the system passes a valid event for the duration of the
        // call.
        let event = unsafe { event.as_ref() };
        let Some(tap_event) = convert(event_type, event) else {
            return Disposition::Pass;
        };
        let out = self.tracker.on_event(tap_event, now, &mut SystemIo);
        self.emit(out.signals);
        out.disposition
    }

    /// The system disabled the tap. Enabling it again without Accessibility
    /// stalls input (F9e), and so does a tap the system keeps disabling.
    fn disabled(&mut self, event_type: CGEventType, now: Instant) {
        self.disables.push_back(now);
        while self
            .disables
            .front()
            .is_some_and(|first| now.duration_since(*first) > DISABLE_WINDOW)
        {
            self.disables.pop_front();
        }
        let storm = self.disables.len() > MAX_DISABLES;
        let trusted = self.shared.trusted.load(Ordering::Acquire);
        if storm || !trusted {
            warn!(
                ?event_type,
                storm, trusted, "event tap disabled; removing it"
            );
            if let Some(run_loop) = CFRunLoop::current() {
                run_loop.stop();
            }
            return;
        }
        warn!(?event_type, "event tap disabled; enabling it again");
        let out = self
            .tracker
            .on_event(TapEvent::TapDisabled, now, &mut SystemIo);
        self.emit(out.signals);
        CGEvent::tap_enable(&self.port, true);
    }

    fn emit(&mut self, signals: Vec<Signal>) {
        for signal in signals {
            (self.shared.on_signal)(signal);
        }
        self.watch();
    }

    /// Runs the watchdog while a gesture is in progress and parks it
    /// otherwise, so an idle tap thread does not wake up.
    fn watch(&mut self) {
        let needed = self.tracker.needs_poll();
        if needed == self.watching {
            return;
        }
        let Some(timer) = &self.watchdog else {
            return;
        };
        let delay = if needed { POLL_INTERVAL } else { PARKED };
        timer.set_next_fire_date(CFAbsoluteTimeGetCurrent() + delay);
        self.watching = needed;
    }

    fn poll(&mut self) {
        if !self.tracker.needs_poll() {
            self.watch();
            return;
        }
        let out = self.tracker.on_poll(poll_state(), Instant::now());
        self.emit(out.signals);
    }
}

/// Runs `f` on the state of the tap thread, if it is set and not in use.
fn with_tap<R>(f: impl FnOnce(&mut TapThread) -> R) -> Option<R> {
    TAP.with(|cell| {
        let mut tap = cell.try_borrow_mut().ok()?;
        tap.as_mut().map(f)
    })
}

unsafe extern "C-unwind" fn on_event(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    // A panic must not unwind into the window server's callback; the event
    // passes unchanged then.
    let handled = panic::catch_unwind(AssertUnwindSafe(|| {
        with_tap(|tap| tap.handle(event_type, event))
    }));
    match handled {
        Ok(Some(Disposition::Swallow)) => std::ptr::null_mut(),
        Ok(Some(Disposition::Pass) | None) => event.as_ptr(),
        Err(_) => {
            error!(?event_type, "event tap callback panicked; event passed");
            event.as_ptr()
        }
    }
}

unsafe extern "C-unwind" fn on_watchdog(_timer: *mut CFRunLoopTimer, _info: *mut c_void) {
    if panic::catch_unwind(AssertUnwindSafe(|| with_tap(TapThread::poll))).is_err() {
        error!("event tap watchdog panicked");
    }
}

fn convert(event_type: CGEventType, event: &CGEvent) -> Option<TapEvent> {
    let field = |field| CGEvent::integer_value_field(Some(event), field);
    let point = || {
        let location = CGEvent::location(Some(event));
        QuartzPoint::new(location.x, location.y)
    };
    let keycode = || u16::try_from(field(CGEventField::KeyboardEventKeycode)).unwrap_or(u16::MAX);
    let modifiers = || {
        let flags = CGEvent::flags(Some(event));
        Modifiers {
            control: flags.contains(CGEventFlags::MaskControl),
            option: flags.contains(CGEventFlags::MaskAlternate),
            command: flags.contains(CGEventFlags::MaskCommand),
            shift: flags.contains(CGEventFlags::MaskShift),
        }
    };
    let tap_event = match event_type {
        CGEventType::KeyDown => TapEvent::KeyDown {
            keycode: keycode(),
            autorepeat: field(CGEventField::KeyboardEventAutorepeat) != 0,
            modifiers: modifiers(),
            point: point(),
        },
        CGEventType::KeyUp => TapEvent::KeyUp {
            keycode: keycode(),
            point: point(),
        },
        CGEventType::FlagsChanged => TapEvent::FlagsChanged {
            modifiers: modifiers(),
            point: point(),
        },
        CGEventType::LeftMouseDown => TapEvent::MouseDown(Button::Left),
        CGEventType::RightMouseDown => TapEvent::MouseDown(Button::Right),
        CGEventType::OtherMouseDown => TapEvent::MouseDown(Button::Other),
        CGEventType::LeftMouseUp => TapEvent::MouseUp(Button::Left),
        CGEventType::RightMouseUp => TapEvent::MouseUp(Button::Right),
        CGEventType::OtherMouseUp => TapEvent::MouseUp(Button::Other),
        CGEventType::MouseMoved => TapEvent::MouseMoved(point()),
        CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => TapEvent::MouseDragged(point()),
        CGEventType::ScrollWheel => TapEvent::Scroll {
            user_data: field(CGEventField::EventSourceUserData),
        },
        _ => return None,
    };
    Some(tap_event)
}

/// The system as the tracker sees it.
struct SystemIo;

impl TapIo for SystemIo {
    fn keyboard_counter(&mut self) -> HidKeys {
        let hid = CGEventSourceStateID::HIDSystemState;
        HidKeys {
            down: CGEventSource::counter_for_event_type(hid, CGEventType::KeyDown),
            up: CGEventSource::counter_for_event_type(hid, CGEventType::KeyUp),
        }
    }

    fn button_held(&mut self) -> bool {
        let hid = CGEventSourceStateID::HIDSystemState;
        [
            CGMouseButton::Left,
            CGMouseButton::Right,
            CGMouseButton::Center,
        ]
        .into_iter()
        .any(|button| CGEventSource::button_state(hid, button))
    }

    fn post_marker(&mut self, user_data: i64) -> bool {
        match post_marker(user_data) {
            Ok(()) => true,
            Err(err) => {
                warn!(%err, "marker not posted");
                false
            }
        }
    }
}

fn poll_state() -> PollState {
    let flags = CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState);
    PollState {
        secure_input: secure_input_enabled(),
        control: flags.contains(CGEventFlags::MaskControl),
        option: flags.contains(CGEventFlags::MaskAlternate),
    }
}

/// The tap of this process in the system's list. A tap that receives every
/// required type wins over one that does not.
fn own_tap() -> Option<ListedTap> {
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
    let mut taps = list
        .into_iter()
        .filter(|info| info.tappingProcess == own)
        .map(|info| ListedTap {
            enabled: info.enabled,
            events_of_interest: info.eventsOfInterest,
        });
    let first = taps.next()?;
    Some(if first.receives_everything() {
        first
    } else {
        taps.find(ListedTap::receives_everything).unwrap_or(first)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn ready() -> TapStatus {
        TapStatus {
            running: true,
            listed: Some(ListedTap {
                enabled: true,
                events_of_interest: REQUIRED_MASK,
            }),
            accessibility: true,
            secure_input: false,
        }
    }

    #[test]
    fn required_mask_has_every_type() {
        for event_type in REQUIRED_TYPES {
            assert_ne!(REQUIRED_MASK & (1 << event_type.0), 0, "{event_type:?}");
        }
        assert_eq!(REQUIRED_MASK.count_ones(), 14);
    }

    #[test]
    fn readiness_needs_a_running_enabled_full_tap_and_accessibility() {
        assert!(ready().ready());
        // Secure keyboard entry hides keys but does not make the tap unready.
        assert!(
            TapStatus {
                secure_input: true,
                ..ready()
            }
            .ready()
        );
        let more = TapStatus {
            listed: Some(ListedTap {
                enabled: true,
                events_of_interest: u64::MAX,
            }),
            ..ready()
        };
        assert!(more.ready());

        let without_key_down = TapStatus {
            listed: Some(ListedTap {
                enabled: true,
                events_of_interest: REQUIRED_MASK & !(1 << CGEventType::KeyDown.0),
            }),
            ..ready()
        };
        let disabled = TapStatus {
            listed: Some(ListedTap {
                enabled: false,
                events_of_interest: REQUIRED_MASK,
            }),
            ..ready()
        };
        for status in [
            without_key_down,
            disabled,
            TapStatus {
                listed: None,
                ..ready()
            },
            TapStatus {
                running: false,
                ..ready()
            },
            TapStatus {
                accessibility: false,
                ..ready()
            },
        ] {
            assert!(!status.ready(), "{status:?}");
        }
    }
}
