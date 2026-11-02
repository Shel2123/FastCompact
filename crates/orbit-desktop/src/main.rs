//! Orbit entry point: creates and wires components.

mod debug;
mod instance;
mod logging;
mod platform;
mod stubs;

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;

use orbit_runtime::{RuntimeConfig, run};
use tracing::{error, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use debug::DebugSettings;
use instance::{InstanceLock, LockError};
use platform::MacPlatform;

fn main() -> ExitCode {
    if let Err(err) = init_logging() {
        warn!(%err, "file logging unavailable; logging to stderr only");
    }
    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(%err, "orbit could not start");
            ExitCode::FAILURE
        }
    }
}

fn start() -> Result<(), Box<dyn Error>> {
    let _lock = match InstanceLock::acquire(&InstanceLock::default_path()?) {
        Ok(lock) => lock,
        Err(LockError::AlreadyRunning) => {
            error!("Orbit is already running; this instance exits");
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    let platform = MacPlatform::new(DebugSettings::from_env())?;
    run(platform, RuntimeConfig::default())?;
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
