//! Orbit entry point.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
mod instance;
#[cfg(target_os = "macos")]
mod macos;

use std::process::ExitCode;

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    macos::run_app()
}

#[cfg(windows)]
fn main() -> ExitCode {
    orbit_windows::run_app()
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() -> ExitCode {
    eprintln!("Orbit supports macOS and Windows.");
    ExitCode::FAILURE
}
