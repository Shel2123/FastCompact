//! A temporary directory for the tests that touch the disk.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{env, fs, process};

/// A fresh directory, removed with its content when dropped.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "orbit-test-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = env::temp_dir().join(name);
        if let Err(err) = fs::create_dir(&path) {
            panic!("temp dir {}: {err}", path.display());
        }
        // Resolve /var → /private/var so the paths are canonical.
        Self(fs::canonicalize(&path).unwrap_or(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
