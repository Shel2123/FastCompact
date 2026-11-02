//! Orbit entry point: creates and wires components.

mod debug;
mod instance;
mod runtime;
mod stubs;

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use orbit_app::messages::AppEvent;
use orbit_app::ports::EventSink;
use orbit_macos::main_thread::MainThreadSender;
use orbit_macos::platform::event_tap::log::InputLog;
use orbit_macos::platform::event_tap::tap::{EventTap, TapOptions};
use orbit_macos::platform::finder_target::FinderSelectionResolver;
use orbit_macos::platform::hotkey::{HotkeyAdapter, forward_signals};
use orbit_macos::platform::permissions::{PermissionPrompt, accessibility_trusted};
use orbit_macos::ui::status_item::status_item;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use debug::DebugSettings;
use instance::{InstanceLock, LockError};
use runtime::{JobsConfig, Runtime};
use stubs::UnavailableExecutor;

fn main() -> ExitCode {
    if let Err(err) = init_logging() {
        warn!(%err, "file logging unavailable; logging to stderr only");
    }
    let Some(mtm) = MainThreadMarker::new() else {
        error!("main() is not on the main thread");
        return ExitCode::FAILURE;
    };
    match run(mtm) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(%err, "orbit could not start");
            ExitCode::FAILURE
        }
    }
}

fn run(mtm: MainThreadMarker) -> Result<(), Box<dyn Error>> {
    let _lock = match InstanceLock::acquire(&InstanceLock::default_path()?) {
        Ok(lock) => lock,
        Err(LockError::AlreadyRunning) => {
            error!("Orbit is already running; this instance exits");
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    let debug = DebugSettings::from_env();
    // Asks for Accessibility on the first start; the tap waits for it.
    info!(
        accessibility = accessibility_trusted(PermissionPrompt::Ask),
        "accessibility checked"
    );

    let log = Arc::new(InputLog::new());
    let sender = MainThreadSender::new(mtm)?;
    let sink: Arc<dyn EventSink> = Arc::new(sender);
    let options = TapOptions {
        watchdog: !debug.no_poll,
    };
    let tap = EventTap::start(
        Arc::clone(&log),
        options,
        forward_signals(Arc::clone(&sink)),
    )?;

    let resolver = {
        let log = Arc::clone(&log);
        let tap = tap.clone();
        move || FinderSelectionResolver::new(log, tap).with_delay(debug.resolve_delay)
    };
    let mut runtime = Runtime::new(
        Arc::clone(&sink),
        resolver,
        Arc::new(UnavailableExecutor),
        JobsConfig::default(),
    )?;
    sender.set_handler(mtm, move |event| {
        if matches!(event, AppEvent::Invoked { .. }) {
            if debug.secure_pulse {
                debug::pulse_secure_input();
            }
            if !debug.main_delay.is_zero() {
                thread::sleep(debug.main_delay);
            }
        }
        runtime.handle(event);
    })?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let _status_item = status_item(mtm);
    let _hotkey = HotkeyAdapter::install(mtm, log, tap, sink)?;

    info!(pid = std::process::id(), "runtime started");
    app.run();
    Ok(())
}

/// Logs to stderr and to `~/Library/Logs/Orbit/orbit.log`. When the file
/// cannot be opened, logs to stderr only and returns the error.
fn init_logging() -> io::Result<()> {
    let stderr = tracing_subscriber::fmt::layer().with_writer(io::stderr);
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

fn log_path() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join("Library/Logs/Orbit/orbit.log"))
}
