//! Orbit entry point: creates and wires components.

mod runtime;
mod stubs;

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use orbit_macos::main_thread::MainThreadSender;
use orbit_macos::ui::status_item::status_item;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use runtime::{JobsConfig, Runtime};
use stubs::{UnavailableExecutor, UnavailableResolver};

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
    let sender = MainThreadSender::new(mtm)?;
    let mut runtime = Runtime::new(
        Arc::new(sender),
        || UnavailableResolver,
        Arc::new(UnavailableExecutor),
        JobsConfig::default(),
    )?;
    sender.set_handler(mtm, move |event| runtime.handle(event))?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let _status_item = status_item(mtm);

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
