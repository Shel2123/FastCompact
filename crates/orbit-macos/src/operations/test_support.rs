//! Helpers for the tests of the executors.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use orbit_app::messages::{JobId, JobSpec};
use orbit_app::ports::{CancelToken, JobContext};
use orbit_domain::operation::OperationSpec;
use orbit_domain::target::{FileFacts, FileIdentity, FileKind, FileTarget, FileVersion};
use orbit_ops::publish::TEMP_PREFIX;
use tempfile::TempDir;
use zip::ZipArchive;

pub fn tempdir() -> TempDir {
    tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"))
}

pub fn ok<T>(result: io::Result<T>, what: &str) -> T {
    result.unwrap_or_else(|error| panic!("{what}: {error}"))
}

/// The target as the resolver reads it, without the package check.
pub fn target(path: &Path) -> FileTarget {
    let metadata = ok(fs::symlink_metadata(path), "stat target");
    let kind = if metadata.file_type().is_symlink() {
        FileKind::Symlink
    } else if metadata.is_dir() {
        FileKind::Directory
    } else {
        FileKind::File
    };
    FileTarget {
        path: path.to_owned(),
        identity: FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        version: FileVersion {
            size: metadata.len(),
            modified: ok(metadata.modified(), "mtime"),
        },
        facts: FileFacts { kind, image: None },
    }
}

/// A ZIP job for `target`.
pub fn spec(target: FileTarget) -> JobSpec {
    let name = target.path.file_name().unwrap_or(OsStr::new("")).to_owned();
    JobSpec {
        id: JobId::new(1),
        destination: OperationSpec::ArchiveZip.destination(&name),
        operation: OperationSpec::ArchiveZip,
        target,
    }
}

pub fn context(cancel: &CancelToken) -> JobContext {
    JobContext::new(cancel.clone(), |_| {})
}

/// Files in `dir` named like a temporary file.
pub fn leftovers(dir: &Path) -> Vec<PathBuf> {
    ok(fs::read_dir(dir), "list")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with(TEMP_PREFIX))
        })
        .collect()
}

/// The entry names of the archive at `path`.
pub fn names(path: &Path) -> Vec<String> {
    let archive = ok(File::open(path), "open archive");
    let archive = ZipArchive::new(archive).unwrap_or_else(|error| panic!("read zip: {error}"));
    archive
        .file_names()
        .map(|name| {
            name.unwrap_or_else(|error| panic!("entry name: {error}"))
                .into_owned()
        })
        .collect()
}
