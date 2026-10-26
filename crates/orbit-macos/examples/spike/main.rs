//! Stage 0 spike: Accessibility trust, reading the Finder target (the
//! selected item by default, the item under the cursor in cursor mode), and
//! the hold-hotkey input route with a non-activating panel.
//!
//! Throwaway code, not part of any public API. Build and launch it as a signed
//! bundle with `scripts/bundle-spike.sh`; results go to
//! `~/Library/Logs/Orbit/spike.log`. See `docs/spike-0.md` for the scenarios.

mod applescript;
mod ax;
mod panel;
mod selection;
mod tap;

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask, NSEventModifierFlags,
    NSMenu, NSMenuItem, NSRunningApplication, NSScreen, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength, NSWorkspace,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString};
use objc2_core_graphics::CGEvent;
use objc2_foundation::{NSBundle, NSPoint, NSString, ns_string};
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const BUNDLE_ID: &str = "com.example.orbit.spike";
const FINDER_BUNDLE_ID: &str = "com.apple.finder";
const BUILD: &str = match option_env!("ORBIT_SPIKE_BUILD") {
    Some(build) => build,
    None => "dev",
};
/// A tap key-down older than this is not taken as the current press.
const TAP_PRESS_MAX_AGE: Duration = Duration::from_secs(1);

fn main() {
    if let Err(err) = init_logging() {
        eprintln!("orbit spike: file logging unavailable: {err}");
    }
    let Some(mtm) = MainThreadMarker::new() else {
        error!("main() is not on the main thread");
        return;
    };
    if let Err(err) = run(mtm) {
        error!("spike stopped: {err}");
    }
}

/// Settings read from `ORBIT_SPIKE_*` environment variables
/// (passed through `open --env` by the bundle script).
#[derive(Debug)]
struct Config {
    mode: Mode,
    hit_delay: Duration,
    attr_delay: Duration,
    select_delay: Duration,
    block_main: Duration,
    point_source: PointSource,
    tap: bool,
    applescript: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Target = the one selected item in the focused Finder file area.
    Selection,
    /// Target = the item under the cursor (fast mode groundwork).
    Cursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointSource {
    /// Cursor position read in the hotkey handler.
    Handler,
    /// Location stored in the key-down event seen by the event tap.
    Tap,
}

impl Config {
    fn from_env() -> Self {
        let millis = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .map_or(Duration::ZERO, Duration::from_millis)
        };
        let var = |name: &str| std::env::var(name).ok();
        let mode = match var("ORBIT_SPIKE_MODE").as_deref() {
            Some("cursor") => Mode::Cursor,
            _ => Mode::Selection,
        };
        let point_source = match var("ORBIT_SPIKE_POINT").as_deref() {
            Some("tap") => PointSource::Tap,
            _ => PointSource::Handler,
        };
        Self {
            mode,
            hit_delay: millis("ORBIT_SPIKE_HIT_DELAY_MS"),
            attr_delay: millis("ORBIT_SPIKE_ATTR_DELAY_MS"),
            select_delay: millis("ORBIT_SPIKE_SELECT_DELAY_MS"),
            block_main: millis("ORBIT_SPIKE_BLOCK_MAIN_MS"),
            point_source,
            tap: point_source == PointSource::Tap || var("ORBIT_SPIKE_TAP").as_deref() == Some("1"),
            applescript: var("ORBIT_SPIKE_APPLESCRIPT").as_deref() != Some("0"),
        }
    }
}

fn init_logging() -> std::io::Result<()> {
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let file = log_path().and_then(|path| {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        OpenOptions::new().create(true).append(true).open(path)
    });
    match file {
        Ok(file) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Mutex::new(file));
            tracing_subscriber::registry()
                .with(stderr)
                .with(file_layer)
                .init();
            Ok(())
        }
        Err(err) => {
            tracing_subscriber::registry().with(stderr).init();
            Err(err)
        }
    }
}

fn log_path() -> std::io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join("Library/Logs/Orbit/spike.log"))
}

fn run(mtm: MainThreadMarker) -> Result<(), String> {
    let config = Config::from_env();
    let bundle = NSBundle::mainBundle();
    let bundle_id = bundle.bundleIdentifier().map(|id| id.to_string());
    info!(
        pid = std::process::id(),
        build = BUILD,
        bundle_path = %bundle.bundlePath(),
        bundle_id = ?bundle_id,
        ?config,
        "spike starting"
    );
    if bundle_id.as_deref() != Some(BUNDLE_ID) {
        warn!("not running from the spike bundle: TCC will attribute access to the parent process");
    }

    let others = other_instances();
    if !others.is_empty() {
        error!(
            ?others,
            "another spike instance is running; exiting without registering hotkeys"
        );
        return Ok(());
    }

    info!(trusted = ax_trusted_with_prompt(), "accessibility trust");

    if config.tap {
        tap::spawn();
    }

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let status_item = status_item(mtm);
    let hotkeys = GlobalHotKeyManager::new().map_err(|err| format!("hotkey manager: {err}"))?;
    let invoke = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
    hotkeys
        .register(invoke)
        .map_err(|err| format!("register Ctrl+Option+Space: {err}"))?;

    let checker = match (config.mode, config.applescript) {
        (Mode::Selection, true) => applescript::Checker::spawn(),
        _ => None,
    };
    let resolver = ax::spawn(
        |event| {
            DispatchQueue::main().exec_async(move || with_spike(|spike| spike.on_resolver(event)));
        },
        checker,
    );

    let mouse_monitor = mouse_monitor();
    if mouse_monitor.is_none() {
        warn!("global mouse-moved monitor was not installed");
    }
    let flags_monitor = flags_monitor();
    if flags_monitor.is_none() {
        warn!("global flags-changed monitor was not installed");
    }

    if !config.block_main.is_zero() {
        spawn_main_blocker(config.block_main);
    }

    let spike = Spike {
        mtm,
        hotkeys,
        invoke,
        escapes: escape_hotkeys(),
        escapes_registered: false,
        resolver,
        panel: panel::RadialPanel::new(mtm),
        gesture: Gesture::Idle,
        next_session: 1,
        config,
        _status_item: status_item,
        _mouse_monitor: mouse_monitor,
        _flags_monitor: flags_monitor,
    };
    SPIKE.with(|cell| *cell.borrow_mut() = Some(spike));

    GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
        // Read the cursor before anything else: this is the invocation point.
        let captured = Captured::now();
        if MainThreadMarker::new().is_none() {
            error!("hotkey event delivered off the main thread");
            return;
        }
        with_spike(|spike| spike.on_hotkey(event, captured));
    }));

    info!("running; select a Finder item (or point at it in cursor mode), hold Ctrl+Option+Space");
    app.run();
    Ok(())
}

thread_local! {
    static SPIKE: RefCell<Option<Spike>> = const { RefCell::new(None) };
}

fn with_spike(f: impl FnOnce(&mut Spike)) {
    SPIKE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => match guard.as_mut() {
            Some(spike) => f(spike),
            None => warn!("event before spike state was ready"),
        },
        Err(_) => error!("re-entrant spike event dropped"),
    });
}

/// Main-thread state of the spike.
struct Spike {
    mtm: MainThreadMarker,
    hotkeys: GlobalHotKeyManager,
    invoke: HotKey,
    escapes: [HotKey; 4],
    escapes_registered: bool,
    resolver: mpsc::Sender<ax::Request>,
    panel: panel::RadialPanel,
    gesture: Gesture,
    next_session: u64,
    config: Config,
    _status_item: Retained<NSStatusItem>,
    _mouse_monitor: Option<Retained<AnyObject>>,
    _flags_monitor: Option<Retained<AnyObject>>,
}

#[derive(Debug)]
enum Gesture {
    Idle,
    /// Waiting for the hit-test (cursor mode) or the selection read.
    Pending {
        session: u64,
        cocoa_point: NSPoint,
        pressed_at: Instant,
    },
    Open {
        session: u64,
        pressed_at: Instant,
        /// The target fixed for the whole gesture; `None` in cursor mode,
        /// where the resolution is only logged.
        target: Option<PathBuf>,
    },
}

/// Cursor position read at the start of the hotkey handler.
#[derive(Debug, Clone, Copy)]
struct Captured {
    at: Instant,
    /// Quartz global coordinates: origin top-left of the primary display.
    quartz: (f64, f64),
    /// Cocoa global coordinates: origin bottom-left of the primary display.
    cocoa: (f64, f64),
}

impl Captured {
    fn now() -> Self {
        let at = Instant::now();
        let event = CGEvent::new(None);
        let quartz = CGEvent::location(event.as_deref());
        let cocoa = NSEvent::mouseLocation();
        Self {
            at,
            quartz: (quartz.x, quartz.y),
            cocoa: (cocoa.x, cocoa.y),
        }
    }
}

impl Spike {
    fn on_hotkey(&mut self, event: GlobalHotKeyEvent, captured: Captured) {
        if event.id == self.invoke.id() {
            match event.state {
                HotKeyState::Pressed => self.on_pressed(captured),
                HotKeyState::Released => self.on_released("space"),
            }
        } else if let Some(escape) = self.escapes.iter().find(|hk| hk.id() == event.id) {
            if event.state == HotKeyState::Pressed {
                info!(mods = ?escape.mods, gesture = ?self.gesture, "escape");
                self.close("escape");
            }
        } else {
            warn!(id = event.id, "unknown hotkey id");
        }
    }

    fn on_pressed(&mut self, captured: Captured) {
        if !matches!(self.gesture, Gesture::Idle) {
            warn!(gesture = ?self.gesture, "pressed while a gesture is active; ignored");
            return;
        }
        let session = self.next_session;
        self.next_session += 1;

        let primary_height = primary_screen_height(self.mtm);
        let converted = primary_height.map(|h| (captured.quartz.0, h - captured.quartz.1));
        let tap_press = tap::take_press()
            .filter(|press| captured.at.duration_since(press.seen_at) < TAP_PRESS_MAX_AGE);
        let frontmost = frontmost_app();
        info!(
            session,
            mode = ?self.config.mode,
            quartz = ?captured.quartz,
            cocoa = ?captured.cocoa,
            cocoa_from_quartz = ?converted,
            tap = ?tap_press.map(|press| (press.point, captured.at.duration_since(press.seen_at))),
            ?frontmost,
            "pressed"
        );

        let (point, cocoa_point) = match (self.config.point_source, tap_press, primary_height) {
            (PointSource::Tap, Some(press), Some(h)) => {
                (press.point, NSPoint::new(press.point.0, h - press.point.1))
            }
            (PointSource::Tap, _, _) => {
                warn!(session, "no tap press recorded; using handler point");
                (
                    captured.quartz,
                    NSPoint::new(captured.cocoa.0, captured.cocoa.1),
                )
            }
            (PointSource::Handler, _, _) => (
                captured.quartz,
                NSPoint::new(captured.cocoa.0, captured.cocoa.1),
            ),
        };

        let request = match self.config.mode {
            Mode::Cursor => ax::Request::Cursor(ax::CursorRequest {
                session,
                point,
                finder_pids: running_pids(FINDER_BUNDLE_ID),
                hit_delay: self.config.hit_delay,
                attr_delay: self.config.attr_delay,
            }),
            Mode::Selection => {
                let finder_pid = frontmost
                    .as_ref()
                    .filter(|(bundle_id, _)| bundle_id.as_deref() == Some(FINDER_BUNDLE_ID))
                    .map(|(_, pid)| *pid);
                let Some(finder_pid) = finder_pid else {
                    info!(session, "refused: Finder is not frontmost; no panel");
                    return;
                };
                ax::Request::Selection(selection::Request {
                    session,
                    finder_pid,
                    pressed_at: tap_press.map_or(captured.at, |press| press.seen_at),
                    delay: self.config.select_delay,
                })
            }
        };
        if self.resolver.send(request).is_err() {
            error!(session, "resolver thread is gone");
            return;
        }
        self.gesture = Gesture::Pending {
            session,
            cocoa_point,
            pressed_at: captured.at,
        };
    }

    /// Ends the gesture. `cause` is the key that was let go: the hotkey's
    /// Space (Carbon `Released`) or one of its modifiers.
    fn on_released(&mut self, cause: &str) {
        match &self.gesture {
            Gesture::Idle => {
                info!(cause, "released while idle; ignored");
                return;
            }
            Gesture::Pending { session, .. } => {
                info!(
                    session,
                    cause, "released before the target was read; gesture closed"
                );
            }
            Gesture::Open {
                session,
                pressed_at,
                target,
            } => info!(
                session,
                ?target,
                cause,
                sector = ?self.panel.selected(),
                frontmost = ?frontmost_bundle_id(),
                tap_inputs = ?tap_inputs_since(*pressed_at),
                "released"
            ),
        }
        self.close("released");
    }

    fn on_resolver(&mut self, event: ax::Event) {
        match event {
            ax::Event::HitTested {
                session,
                result,
                elapsed,
            } => match self.gesture {
                Gesture::Pending {
                    session: current,
                    cocoa_point,
                    pressed_at,
                } if current == session => {
                    let since_press = pressed_at.elapsed();
                    match result {
                        Ok(hit) => {
                            info!(
                                session,
                                ?hit,
                                ?elapsed,
                                ?since_press,
                                "hit-test ok; showing panel"
                            );
                            self.open(session, cocoa_point, pressed_at, None);
                        }
                        Err(reason) => {
                            info!(session, %reason, ?elapsed, "hit-test rejected; no panel");
                            self.gesture = Gesture::Idle;
                        }
                    }
                }
                _ => info!(session, gesture = ?self.gesture, "stale hit-test ignored"),
            },
            ax::Event::Resolved {
                session,
                resolution,
                elapsed,
            } => {
                let current =
                    matches!(self.gesture, Gesture::Open { session: s, .. } if s == session);
                info!(session, ?resolution, ?elapsed, current, "resolution");
            }
            ax::Event::Selected {
                session,
                finder_pid,
                outcome,
                snapshot,
                elapsed,
            } => match self.gesture {
                Gesture::Pending {
                    session: current,
                    cocoa_point,
                    pressed_at,
                } if current == session => {
                    let since_press = pressed_at.elapsed();
                    info!(session, %outcome, ?snapshot, ?elapsed, ?since_press, "selection");
                    let selection::Outcome::Resolved(path) = outcome else {
                        info!(session, "refused: not exactly one selected file; no panel");
                        self.gesture = Gesture::Idle;
                        return;
                    };
                    self.open(session, cocoa_point, pressed_at, Some(path));
                    let verify = ax::Request::Verify(selection::Verify {
                        session,
                        finder_pid,
                        expected: snapshot,
                    });
                    if self.resolver.send(verify).is_err() {
                        error!(session, "resolver thread is gone");
                    }
                }
                _ => info!(session, %outcome, gesture = ?self.gesture, "stale selection ignored"),
            },
        }
    }

    fn open(
        &mut self,
        session: u64,
        cocoa_point: NSPoint,
        pressed_at: Instant,
        target: Option<PathBuf>,
    ) {
        let center = self.panel.show(self.mtm, cocoa_point);
        info!(
            session,
            ?target,
            ?cocoa_point,
            ?center,
            frontmost = ?frontmost_bundle_id(),
            "panel shown"
        );
        self.register_escapes();
        self.gesture = Gesture::Open {
            session,
            pressed_at,
            target,
        };
    }

    fn close(&mut self, reason: &str) {
        self.panel.hide();
        self.unregister_escapes();
        info!(reason, gesture = ?self.gesture, "gesture closed");
        self.gesture = Gesture::Idle;
    }

    fn register_escapes(&mut self) {
        if self.escapes_registered {
            return;
        }
        for escape in &self.escapes {
            if let Err(err) = self.hotkeys.register(*escape) {
                warn!(mods = ?escape.mods, %err, "escape hotkey not registered");
            }
        }
        self.escapes_registered = true;
    }

    fn unregister_escapes(&mut self) {
        if !self.escapes_registered {
            return;
        }
        for escape in &self.escapes {
            if let Err(err) = self.hotkeys.unregister(*escape) {
                warn!(mods = ?escape.mods, %err, "escape hotkey not unregistered");
            }
        }
        self.escapes_registered = false;
    }

    fn on_mouse_moved(&mut self, cursor: NSPoint) {
        if matches!(self.gesture, Gesture::Open { .. }) {
            self.panel.track(cursor);
        }
    }

    fn on_flags_changed(&mut self, flags: NSEventModifierFlags) {
        if matches!(self.gesture, Gesture::Idle) {
            return;
        }
        let control = flags.contains(NSEventModifierFlags::Control);
        let option = flags.contains(NSEventModifierFlags::Option);
        info!(control, option, gesture = ?self.gesture, "modifiers changed");
        // Carbon reports `Released` only when Space goes up, so letting go of
        // a modifier first must end the gesture here.
        if !(control && option) {
            self.on_released("modifier");
        }
    }
}

/// Esc with every modifier combination the user can hold during the gesture.
fn escape_hotkeys() -> [HotKey; 4] {
    [
        HotKey::new(None, Code::Escape),
        HotKey::new(Some(Modifiers::CONTROL), Code::Escape),
        HotKey::new(Some(Modifiers::ALT), Code::Escape),
        HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Escape),
    ]
}

fn mouse_monitor() -> Option<Retained<AnyObject>> {
    let handler = block2::RcBlock::new(|_event| {
        let cursor = NSEvent::mouseLocation();
        with_spike(|spike| spike.on_mouse_moved(cursor));
    });
    NSEvent::addGlobalMonitorForEventsMatchingMask_handler(NSEventMask::MouseMoved, &handler)
}

/// Logs modifier changes during a gesture, so the release order of the
/// hotkey keys can be read from the log (scenarios 7a and 8).
fn flags_monitor() -> Option<Retained<AnyObject>> {
    let handler = block2::RcBlock::new(|_event| {
        let flags = NSEvent::modifierFlags_class();
        with_spike(|spike| spike.on_flags_changed(flags));
    });
    NSEvent::addGlobalMonitorForEventsMatchingMask_handler(NSEventMask::FlagsChanged, &handler)
}

/// Blocks the main thread periodically to delay hotkey delivery (scenario 6b).
fn spawn_main_blocker(duration: Duration) {
    let spawned = thread::Builder::new()
        .name("orbit-spike-blocker".into())
        .spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(6));
                DispatchQueue::main().exec_async(move || {
                    info!(?duration, "main thread blocked");
                    thread::sleep(duration);
                    info!("main thread unblocked");
                });
            }
        });
    if let Err(err) = spawned {
        error!(%err, "main blocker thread not started");
    }
}

fn status_item(mtm: MainThreadMarker) -> Retained<NSStatusItem> {
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
            ns_string!("Quit Orbit Spike"),
            Some(sel!(terminate:)),
            ns_string!("q"),
        )
    };
    menu.addItem(&quit);
    item.setMenu(Some(&menu));
    item
}

fn ax_trusted_with_prompt() -> bool {
    // SAFETY: reading an immutable framework constant.
    let key: &CFString = unsafe { objc2_application_services::kAXTrustedCheckOptionPrompt };
    let options: CFRetained<CFDictionary<CFString, CFBoolean>> =
        CFDictionary::from_slices(&[key], &[CFBoolean::new(true)]);
    // SAFETY: the options dictionary has CFString keys and CFBoolean values,
    // as the function expects.
    unsafe { objc2_application_services::AXIsProcessTrustedWithOptions(Some(options.as_opaque())) }
}

fn running_pids(bundle_id: &str) -> Vec<i32> {
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(bundle_id))
        .iter()
        .map(|app| app.processIdentifier())
        .collect()
}

fn other_instances() -> Vec<i32> {
    let own = NSRunningApplication::currentApplication().processIdentifier();
    running_pids(BUNDLE_ID)
        .into_iter()
        .filter(|pid| *pid != own)
        .collect()
}

/// Tap inputs seen after `since`, each with its offset from `since`. Shows
/// whether a click or key-down came before or after the selection read.
fn tap_inputs_since(since: Instant) -> Vec<(tap::Input, Duration)> {
    tap::inputs_since(since)
        .into_iter()
        .map(|input| (input.input, input.seen_at.duration_since(since)))
        .collect()
}

fn frontmost_bundle_id() -> Option<String> {
    frontmost_app().and_then(|(bundle_id, _)| bundle_id)
}

/// Bundle id and pid of the frontmost application.
fn frontmost_app() -> Option<(Option<String>, i32)> {
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let bundle_id = app.bundleIdentifier().map(|id| id.to_string());
    Some((bundle_id, app.processIdentifier()))
}

fn primary_screen_height(mtm: MainThreadMarker) -> Option<f64> {
    NSScreen::screens(mtm)
        .iter()
        .next()
        .map(|screen| screen.frame().size.height)
}
