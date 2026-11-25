//! Where Orbit keeps its log and its instance lock.

use std::io;
use std::path::PathBuf;

/// `~/Library/Logs/Orbit/orbit.log` on macOS,
/// `%LOCALAPPDATA%\Orbit\Logs\orbit.log` on Windows.
pub fn log_path() -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let path = env_dir("HOME")?.join("Library/Logs/Orbit/orbit.log");
    #[cfg(windows)]
    let path = env_dir("LOCALAPPDATA")?.join(r"Orbit\Logs\orbit.log");
    Ok(path)
}

/// `~/Library/Application Support/Orbit/instance.lock` on macOS,
/// `%LOCALAPPDATA%\Orbit\instance.lock` on Windows.
pub fn lock_path() -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let path = env_dir("HOME")?.join("Library/Application Support/Orbit/instance.lock");
    #[cfg(windows)]
    let path = env_dir("LOCALAPPDATA")?.join(r"Orbit\instance.lock");
    Ok(path)
}

fn env_dir(name: &str) -> io::Result<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{name} is not set")))
}
