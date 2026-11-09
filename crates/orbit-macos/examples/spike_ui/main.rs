//! Stage 5 spike: the radial menu UI without Finder, tap or controller.
//!
//! Hold Ctrl+Option+Space: the menu appears at the cursor, moving the mouse
//! highlights sectors, releasing Space (or Ctrl/Option) closes it and logs the
//! slot under the cursor. See `docs/spike-5.md` for the questions G1 to G10.
//!
//! Environment (all optional):
//! - `ORBIT_SPIKE_UI_BACKGROUND`: `glass` (default), `vibrancy`, `plain`
//! - `ORBIT_SPIKE_UI_ANIM`: `spring`, `ease`, `none` (default)
//! - `ORBIT_SPIKE_UI_DURATION_MS`: appearance duration (default 700)
//! - `ORBIT_SPIKE_UI_BOUNCE`: spring bounce, 0 to 1 (default 0.5)
//! - `ORBIT_SPIKE_UI_HIGHLIGHT_MS`: highlight transition (default 70)
//! - `ORBIT_SPIKE_UI_UNAVAILABLE`: comma-separated slots drawn unavailable
//! - `ORBIT_SPIKE_UI_DUMP`: `1` logs the layer tree once at start

mod view;

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::Instant;

use dispatch2::DispatchQueue;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask, NSEventModifierFlags,
    NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::ns_string;
use orbit_domain::action::MENU_LAYOUT;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use view::{Appearance, Background, Config, MenuPanel};

fn main() {
    if let Err(err) = init_logging() {
        warn!(%err, "file logging unavailable; logging to stderr only");
    }
    let Some(mtm) = MainThreadMarker::new() else {
        error!("not on the main thread");
        return;
    };
    let config = config_from_env();
    info!(?config, pid = std::process::id(), "spike_ui starting");

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let status_item = status_item(mtm);

    let hotkeys = match GlobalHotKeyManager::new() {
        Ok(hotkeys) => hotkeys,
        Err(err) => {
            error!(%err, "hotkey manager");
            return;
        }
    };
    let invoke = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
    if let Err(err) = hotkeys.register(invoke) {
        error!(%err, "register Ctrl+Option+Space");
        return;
    }

    let build_started = Instant::now();
    let panel = MenuPanel::new(mtm, config);
    info!(elapsed = ?build_started.elapsed(), "panel built");
    if std::env::var("ORBIT_SPIKE_UI_DUMP").is_ok_and(|value| value == "1") {
        panel.dump();
    }

    let mouse = mouse_monitor();
    let flags = flags_monitor();
    if mouse.is_none() || flags.is_none() {
        warn!("global monitors were not installed");
    }
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(State {
            mtm,
            panel,
            open: None,
            _hotkeys: hotkeys,
            _status_item: status_item,
            _monitors: (mouse, flags),
        });
    });

    let hotkey_id = invoke.id();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        // The clock starts before anything else: it is the hotkey latency.
        let started = Instant::now();
        if event.id() != hotkey_id {
            return;
        }
        match event.state() {
            HotKeyState::Pressed => with_state(|state| state.open(started)),
            HotKeyState::Released => with_state(|state| state.close("space released")),
        }
    }));

    info!("running; hold Ctrl+Option+Space");
    app.run();
}

struct State {
    mtm: MainThreadMarker,
    panel: MenuPanel,
    open: Option<Instant>,
    _hotkeys: GlobalHotKeyManager,
    _status_item: Retained<NSStatusItem>,
    _monitors: (Option<Retained<AnyObject>>, Option<Retained<AnyObject>>),
}

impl State {
    fn open(&mut self, started: Instant) {
        if self.open.is_some() {
            // Auto-repeat of the held key.
            return;
        }
        self.open = Some(started);
        let cursor = NSEvent::mouseLocation();
        self.panel.show(self.mtm, cursor);
        let shown = started.elapsed();
        info!(?shown, dark = self.panel.dark(), "show returned");
        // Next turn of the main run loop: the transaction has been committed.
        DispatchQueue::main().exec_async(move || {
            info!(elapsed = ?started.elapsed(), "next run loop turn after show");
        });
    }

    fn close(&mut self, reason: &str) {
        if self.open.take().is_none() {
            return;
        }
        let slot = self.panel.selected();
        self.panel.hide();
        match slot {
            Some(slot) => info!(
                reason,
                slot,
                item = ?MENU_LAYOUT[slot],
                available = self.panel.available(slot),
                "menu closed"
            ),
            None => info!(reason, "menu closed without a choice"),
        }
    }

    fn moved(&mut self, cursor: objc2_foundation::NSPoint) {
        if self.open.is_some() {
            self.panel.track(cursor);
        }
    }

    fn flags(&mut self, flags: NSEventModifierFlags) {
        if self.open.is_none() {
            return;
        }
        let held = flags.contains(NSEventModifierFlags::Control)
            && flags.contains(NSEventModifierFlags::Option);
        if !held {
            self.close("modifier released");
        }
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state(f: impl FnOnce(&mut State)) {
    if MainThreadMarker::new().is_none() {
        error!("event delivered off the main thread");
        return;
    }
    STATE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => match guard.as_mut() {
            Some(state) => f(state),
            None => warn!("event before the state was ready"),
        },
        Err(_) => error!("re-entrant event dropped"),
    });
}

fn mouse_monitor() -> Option<Retained<AnyObject>> {
    let handler = block2::RcBlock::new(|_event: NonNull<NSEvent>| {
        let cursor = NSEvent::mouseLocation();
        with_state(|state| state.moved(cursor));
    });
    NSEvent::addGlobalMonitorForEventsMatchingMask_handler(NSEventMask::MouseMoved, &handler)
}

fn flags_monitor() -> Option<Retained<AnyObject>> {
    let handler = block2::RcBlock::new(|event: NonNull<NSEvent>| {
        // SAFETY: AppKit passes a valid event for the duration of the call.
        let flags = unsafe { event.as_ref() }.modifierFlags();
        with_state(|state| state.flags(flags));
    });
    NSEvent::addGlobalMonitorForEventsMatchingMask_handler(NSEventMask::FlagsChanged, &handler)
}

fn config_from_env() -> Config {
    let text = |name: &str| std::env::var(name).ok();
    let number = |name: &str, default: f64| {
        text(name)
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(default)
    };
    let background = match text("ORBIT_SPIKE_UI_BACKGROUND").as_deref() {
        Some("vibrancy") => Background::Vibrancy,
        Some("plain") => Background::Plain,
        _ => Background::Glass,
    };
    let appearance = match text("ORBIT_SPIKE_UI_ANIM").as_deref() {
        Some("spring") => Appearance::Spring,
        Some("ease") => Appearance::Ease,
        _ => Appearance::None,
    };
    let unavailable = text("ORBIT_SPIKE_UI_UNAVAILABLE")
        .map(|list| {
            list.split(',')
                .filter_map(|slot| slot.trim().parse::<usize>().ok())
                .collect()
        })
        .unwrap_or_default();
    Config {
        background,
        appearance,
        unavailable,
        bounce: number("ORBIT_SPIKE_UI_BOUNCE", 0.5),
        duration: number("ORBIT_SPIKE_UI_DURATION_MS", 700.0) / 1000.0,
        highlight_ms: number("ORBIT_SPIKE_UI_HIGHLIGHT_MS", 70.0),
    }
}

fn status_item(mtm: MainThreadMarker) -> Retained<NSStatusItem> {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        button.setTitle(ns_string!("◎ UI"));
    }
    let menu = NSMenu::new(mtm);
    // SAFETY: `terminate:` is an NSApplication action; with no target it goes
    // up the responder chain to NSApp.
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Quit Orbit Spike UI"),
            Some(sel!(terminate:)),
            ns_string!("q"),
        )
    };
    menu.addItem(&quit);
    item.setMenu(Some(&menu));
    item
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
