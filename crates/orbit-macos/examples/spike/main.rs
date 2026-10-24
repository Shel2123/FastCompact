//! Stage 0 spike: Accessibility trust, hit-testing the Finder item under the
//! cursor, and the hold-hotkey input route with a non-activating panel.
//!
//! Throwaway code, not part of any public API. Build and launch it as a signed
//! bundle with `scripts/bundle-spike.sh`; results go to
//! `~/Library/Logs/Orbit/spike.log`. See `docs/spike-0.md` for the scenarios.

mod ax;
mod panel;
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
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask, NSMenu, NSMenuItem,
    NSRunningApplication, NSScreen, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
    NSWorkspace,
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
    hit_delay: Duration,
    attr_delay: Duration,
    block_main: Duration,
    point_source: PointSource,
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
        let point_source = match std::env::var("ORBIT_SPIKE_POINT").as_deref() {
            Ok("tap") => PointSource::Tap,
            _ => PointSource::Handler,
        };
        Self {
            hit_delay: millis("ORBIT_SPIKE_HIT_DELAY_MS"),
            attr_delay: millis("ORBIT_SPIKE_ATTR_DELAY_MS"),
            block_main: millis("ORBIT_SPIKE_BLOCK_MAIN_MS"),
            point_source,
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

    if config.point_source == PointSource::Tap {
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

    let resolver = ax::spawn(|event| {
        DispatchQueue::main().exec_async(move || with_spike(|spike| spike.on_resolver(event)));
    });

    let mouse_monitor = mouse_monitor();
    if mouse_monitor.is_none() {
        warn!("global mouse-moved monitor was not installed");
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

    info!("running; hold Ctrl+Option+Space over a Finder item");
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
}

#[derive(Debug, Clone, Copy)]
enum Gesture {
    Idle,
    HitTesting {
        session: u64,
        cocoa_point: NSPoint,
        pressed_at: Instant,
    },
    Open {
        session: u64,
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
                HotKeyState::Released => self.on_released(),
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
        let tap_press = tap::take_press();
        info!(
            session,
            quartz = ?captured.quartz,
            cocoa = ?captured.cocoa,
            cocoa_from_quartz = ?converted,
            tap = ?tap_press.map(|press| (press.point, captured.at.duration_since(press.seen_at))),
            frontmost = ?frontmost_bundle_id(),
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

        let request = ax::Request {
            session,
            point,
            finder_pids: running_pids(FINDER_BUNDLE_ID),
            hit_delay: self.config.hit_delay,
            attr_delay: self.config.attr_delay,
        };
        if self.resolver.send(request).is_err() {
            error!(session, "resolver thread is gone");
            return;
        }
        self.gesture = Gesture::HitTesting {
            session,
            cocoa_point,
            pressed_at: captured.at,
        };
    }

    fn on_released(&mut self) {
        match self.gesture {
            Gesture::Idle => info!("released while idle; ignored"),
            Gesture::HitTesting { session, .. } => {
                info!(session, "released before hit-test finished; gesture closed");
                self.close("released");
            }
            Gesture::Open { session } => {
                info!(
                    session,
                    sector = ?self.panel.selected(),
                    frontmost = ?frontmost_bundle_id(),
                    "released"
                );
                self.close("released");
            }
        }
    }

    fn on_resolver(&mut self, event: ax::Event) {
        match event {
            ax::Event::HitTested {
                session,
                result,
                elapsed,
            } => match self.gesture {
                Gesture::HitTesting {
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
                            self.open(session, cocoa_point);
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
                let current = matches!(self.gesture, Gesture::Open { session: s } if s == session);
                info!(session, ?resolution, ?elapsed, current, "resolution");
            }
        }
    }

    fn open(&mut self, session: u64, cocoa_point: NSPoint) {
        let center = self.panel.show(self.mtm, cocoa_point);
        info!(
            session,
            ?cocoa_point,
            ?center,
            frontmost = ?frontmost_bundle_id(),
            "panel shown"
        );
        self.register_escapes();
        self.gesture = Gesture::Open { session };
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

fn frontmost_bundle_id() -> Option<String> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|app| app.bundleIdentifier())
        .map(|id| id.to_string())
}

fn primary_screen_height(mtm: MainThreadMarker) -> Option<f64> {
    NSScreen::screens(mtm)
        .iter()
        .next()
        .map(|screen| screen.frame().size.height)
}
