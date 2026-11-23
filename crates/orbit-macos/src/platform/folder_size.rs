//! The size of a folder: the sum of the lengths of what it holds, as Finder
//! counts it in Get Info.
//!
//! - Symbolic links count with their own length and are not followed.
//! - A file with several hard links counts once.
//! - Folders on another device (mounted volumes) are not entered, like
//!   `du -x`.
//! - What cannot be read is counted as unreadable and skipped.
//!
//! The walk reads the disk and may take long, so it runs off the main
//! thread. It stops at the next entry once `cancel` is raised; a single
//! call that blocks (a slow network volume) is not interrupted.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// How many entries go by between two calls of `progress`.
const PROGRESS_EVERY: u64 = 256;

/// The result of a whole walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FolderSize {
    /// The sum of the lengths, in bytes.
    pub bytes: u64,
    /// Folders and entries that could not be read; their size is missing,
    /// so `bytes` is a lower bound when this is not zero.
    pub unreadable: u64,
}

/// How a walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    Done(FolderSize),
    Cancelled,
}

/// Walks `root` and sums what it holds. `progress` gets the running sum
/// every few hundred entries.
pub fn folder_size(root: &Path, cancel: &AtomicBool, mut progress: impl FnMut(u64)) -> Walk {
    let mut size = FolderSize::default();
    let device = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata.dev(),
        Err(_) => {
            size.unreadable = 1;
            return Walk::Done(size);
        }
    };
    let mut linked: HashSet<(u64, u64)> = HashSet::new();
    let mut folders: Vec<PathBuf> = vec![root.to_owned()];
    let mut seen: u64 = 0;
    while let Some(folder) = folders.pop() {
        let Ok(entries) = fs::read_dir(&folder) else {
            size.unreadable += 1;
            continue;
        };
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                return Walk::Cancelled;
            }
            seen += 1;
            if seen.is_multiple_of(PROGRESS_EVERY) {
                progress(size.bytes);
            }
            // `DirEntry::metadata` does not follow symbolic links.
            let Ok((entry, metadata)) = entry.and_then(|entry| {
                let metadata = entry.metadata()?;
                Ok((entry, metadata))
            }) else {
                size.unreadable += 1;
                continue;
            };
            let kind = metadata.file_type();
            if kind.is_dir() {
                if metadata.dev() == device {
                    folders.push(entry.path());
                }
            } else if kind.is_file() || kind.is_symlink() {
                let once = metadata.nlink() <= 1
                    || kind.is_symlink()
                    || linked.insert((metadata.dev(), metadata.ino()));
                if once {
                    size.bytes = size.bytes.saturating_add(metadata.len());
                }
            }
        }
    }
    Walk::Done(size)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::platform::test_dir::TempDir;

    fn walk(root: &Path) -> Walk {
        folder_size(root, &AtomicBool::new(false), |_| {})
    }

    fn done(bytes: u64, unreadable: u64) -> Walk {
        Walk::Done(FolderSize { bytes, unreadable })
    }

    #[test]
    fn nested_files_add_up() -> std::io::Result<()> {
        let dir = TempDir::new();
        fs::write(dir.join("a"), [0; 10])?;
        fs::create_dir_all(dir.join("x/y"))?;
        fs::write(dir.join("x/b"), [0; 200])?;
        fs::write(dir.join("x/y/c"), [0; 3000])?;
        assert_eq!(walk(dir.path()), done(3210, 0));
        Ok(())
    }

    #[test]
    fn an_empty_folder_is_zero() {
        let dir = TempDir::new();
        assert_eq!(walk(dir.path()), done(0, 0));
    }

    #[test]
    fn a_link_counts_itself_not_what_it_points_to() -> std::io::Result<()> {
        let outside = TempDir::new();
        let big = outside.join("big");
        fs::write(&big, vec![0; 100_000])?;
        let dir = TempDir::new();
        let link = dir.join("link");
        symlink(&big, &link)?;
        let own = fs::symlink_metadata(&link)?.len();
        assert!(own < 1000);
        assert_eq!(walk(dir.path()), done(own, 0));
        Ok(())
    }

    #[test]
    fn a_hard_linked_file_counts_once() -> std::io::Result<()> {
        let dir = TempDir::new();
        fs::write(dir.join("a"), [0; 500])?;
        fs::hard_link(dir.join("a"), dir.join("b"))?;
        assert_eq!(walk(dir.path()), done(500, 0));
        Ok(())
    }

    #[test]
    fn a_raised_cancel_stops_the_walk() -> std::io::Result<()> {
        let dir = TempDir::new();
        fs::write(dir.join("a"), [0; 1])?;
        let cancel = AtomicBool::new(true);
        assert_eq!(folder_size(dir.path(), &cancel, |_| {}), Walk::Cancelled);
        Ok(())
    }

    #[test]
    fn an_unreadable_folder_is_counted_and_skipped() -> std::io::Result<()> {
        let dir = TempDir::new();
        fs::write(dir.join("a"), [0; 7])?;
        let closed = dir.join("closed");
        fs::create_dir(&closed)?;
        fs::write(closed.join("hidden"), [0; 100])?;
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000))?;
        // Root reads it anyway; then there is nothing to check.
        let readable = fs::read_dir(&closed).is_ok();
        let result = walk(dir.path());
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755))?;
        if !readable {
            assert_eq!(result, done(7, 1));
        }
        Ok(())
    }

    #[test]
    fn progress_reports_a_growing_sum() -> std::io::Result<()> {
        let dir = TempDir::new();
        for index in 0..600 {
            fs::write(dir.join(&format!("f{index}")), [0; 3])?;
        }
        let mut seen = Vec::new();
        let result = folder_size(dir.path(), &AtomicBool::new(false), |bytes| {
            seen.push(bytes);
        });
        assert_eq!(result, done(1800, 0));
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen.windows(2).all(|pair| pair[0] <= pair[1]), "{seen:?}");
        Ok(())
    }
}
