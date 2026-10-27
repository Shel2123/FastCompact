//! One Orbit at a time.
//!
//! Two instances would both register the hotkey and both run an event tap,
//! so the second one stops at start. The guard is an advisory lock on a
//! file; the system releases it when the process ends, however it ends.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Why the lock was not taken.
#[derive(Debug, Error)]
pub enum LockError {
    /// Another instance holds the lock.
    #[error("Orbit is already running")]
    AlreadyRunning,
    /// The lock file could not be opened or locked.
    #[error("instance lock {path}: {source}")]
    Io {
        /// The lock file.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
}

/// The lock of the running instance; released when dropped.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Takes the lock at `path`, creating the file and its folder.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        let io_error = |source| LockError::Io {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io_error)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(io_error)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(LockError::AlreadyRunning),
            Err(TryLockError::Error(source)) => Err(io_error(source)),
        }
    }

    /// `~/Library/Application Support/Orbit/instance.lock`.
    pub fn default_path() -> io::Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        Ok(PathBuf::from(home).join("Library/Application Support/Orbit/instance.lock"))
    }
}

#[cfg(test)]
mod tests {
    use std::{env, process};

    use super::*;

    #[test]
    fn second_lock_fails_until_the_first_is_dropped() {
        let dir = env::temp_dir().join(format!("orbit-lock-{}", process::id()));
        let path = dir.join("nested/instance.lock");
        let first = InstanceLock::acquire(&path);
        assert!(first.is_ok(), "{first:?}");
        assert!(matches!(
            InstanceLock::acquire(&path),
            Err(LockError::AlreadyRunning)
        ));
        drop(first);
        assert!(InstanceLock::acquire(&path).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unusable_path_is_an_io_error() {
        let file = env::temp_dir().join(format!("orbit-lock-file-{}", process::id()));
        assert!(fs::write(&file, b"").is_ok());
        // A file where the folder should be.
        assert!(matches!(
            InstanceLock::acquire(&file.join("instance.lock")),
            Err(LockError::Io { .. })
        ));
        let _ = fs::remove_file(file);
    }
}
