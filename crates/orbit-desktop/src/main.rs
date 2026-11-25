//! Orbit entry point: sets up logging, makes sure only one Orbit runs and
//! hands over to the platform of this system.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(target_os = "macos", windows))]
mod instance;
#[cfg(any(target_os = "macos", windows))]
mod logging;
#[cfg(any(target_os = "macos", windows))]
mod paths;

use std::process::ExitCode;

#[cfg(any(target_os = "macos", windows))]
type StartError = Box<dyn std::error::Error + Send + Sync>;

#[cfg(any(target_os = "macos", windows))]
fn main() -> ExitCode {
    use tracing::{error, warn};

    if let Err(err) = logging::init(paths::log_path()) {
        warn!(%err, "file logging unavailable; logging to stderr only");
    }
    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(%err, "orbit could not start");
            platform::report_failure(&err.to_string());
            ExitCode::FAILURE
        }
    }
}

#[cfg(any(target_os = "macos", windows))]
fn start() -> Result<(), StartError> {
    use instance::{InstanceLock, LockError};

    let _lock = match InstanceLock::acquire(&paths::lock_path()?) {
        Ok(lock) => lock,
        Err(LockError::AlreadyRunning) => {
            tracing::error!("Orbit is already running; this instance exits");
            platform::report_already_running();
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    platform::run()
}

#[cfg(target_os = "macos")]
mod platform {
    use orbit_macos::{MacOptions, MacPlatform};
    use orbit_runtime::{RuntimeConfig, run as run_runtime};

    use crate::StartError;

    pub fn run() -> Result<(), StartError> {
        run_runtime(
            MacPlatform::new(MacOptions::from_env())?,
            RuntimeConfig::default(),
        )?;
        Ok(())
    }

    /// The log already has the error; there is nothing else to show it in.
    pub fn report_failure(_: &str) {}

    /// The log already says so; the running Orbit stays as it is.
    pub fn report_already_running() {}
}

#[cfg(windows)]
mod platform {
    use orbit_runtime::{RuntimeConfig, run as run_runtime};
    use orbit_windows::{WindowsPlatform, show_error, show_notice};

    use crate::StartError;

    pub fn run() -> Result<(), StartError> {
        run_runtime(WindowsPlatform::new()?, RuntimeConfig::default())?;
        Ok(())
    }

    /// A release build has no console, so the error is also shown in a
    /// message box.
    pub fn report_failure(message: &str) {
        show_error(&format!("Orbit could not start.\n\n{message}"));
    }

    /// Without this a second start would look like nothing happened.
    pub fn report_already_running() {
        show_notice("Orbit is already running. Use its icon in the notification area to exit it.");
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() -> ExitCode {
    eprintln!("Orbit supports macOS and Windows.");
    ExitCode::FAILURE
}
