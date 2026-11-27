//! Publishing a finished file under a free name, never over another file.
//!
//! An operation writes its result to a temporary file from [`create_temp`]
//! in the destination folder. [`commit`] then:
//!
//! 1. flushes the buffered writer; a failed flush is returned, not lost in
//!    `Drop`;
//! 2. syncs the file to the disk;
//! 3. renames it to the first free name of the candidates. The rename never
//!    replaces anything: a taken name, whether a file, a folder or a dangling
//!    symlink, gives `AlreadyExists`, and the same temporary file is tried
//!    under the next candidate, so nothing is written twice.
//!
//! The commit point is a successful rename. Before it, every failure and a
//! cancellation delete the temporary file. After it, the job has succeeded,
//! even if a cancellation came at the same time: the file is already in
//! place under its name.
//!
//! A volume that cannot rename without replacing (exFAT on macOS) gets no
//! fallback, such as reserving the name first: the job stops and the user is
//! told that Orbit can't create the file safely there.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

use orbit_app::messages::JobError;
use orbit_domain::operation::{Destination, NameCandidates};
use tempfile::{NamedTempFile, PersistError};
use tracing::{debug, warn};

/// Start of the name of every temporary file: a dot file, which Finder hides.
pub const TEMP_PREFIX: &str = ".orbit-";
/// End of the name of every temporary file.
pub const TEMP_SUFFIX: &str = ".tmp";

/// What the user is told when the volume can't rename without replacing.
///
/// It is the same on every such volume: Orbit does not look up the type of
/// the filesystem.
const NO_SAFE_PUBLISH: &str = "Orbit can't safely create a ZIP on this disk";

/// Creates an empty temporary file in `dir`, the folder of the result.
///
/// The file is hidden by its name and deleted when it is dropped. On Unix it
/// gets the mode `0o666` less the umask, like a file saved by Finder, not
/// the `0o600` of a usual temporary file, because [`commit`] publishes it as
/// it is.
///
/// # Errors
///
/// [`JobError::Io`] when the file can't be created, for example in a folder
/// that is read-only.
pub fn create_temp(dir: &Path) -> Result<NamedTempFile, JobError> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(TEMP_PREFIX).suffix(TEMP_SUFFIX);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    builder.tempfile_in(dir).map_err(|error| {
        io_error(
            &error,
            &format!("couldn't create a file in {}", folder(dir)),
        )
    })
}

/// Makes the written temporary file durable and publishes it under the
/// first free name of `destination` in `dir`; returns its path.
///
/// `output` is the writer around a file from [`create_temp`] in `dir`. A
/// file written by someone else, such as a child process, is passed as
/// `BufWriter::new(temp)`: the flush then has nothing to do.
///
/// `cancelled` is asked before the flush and before every rename. On every
/// error the temporary file is deleted.
///
/// # Errors
///
/// - [`JobError::Cancelled`] when `cancelled` says so before the commit
///   point;
/// - [`JobError::NoFreeName`] when every candidate is taken;
/// - [`JobError::Io`] when the flush, the sync or a rename fails for any
///   other reason, including a name that is too long; kind
///   [`ErrorKind::Unsupported`] when the volume can't rename without
///   replacing;
/// - [`JobError::Internal`] for [`Destination::Exact`], which only Rename
///   uses and which is not published this way.
pub fn commit(
    output: BufWriter<NamedTempFile>,
    dir: &Path,
    destination: &Destination,
    cancelled: impl Fn() -> bool,
) -> Result<PathBuf, JobError> {
    commit_with(output, File::sync_all, dir, destination, cancelled)
}

/// [`commit`] with the sync of the file passed in, so that tests can fail it.
fn commit_with<F: Write>(
    output: BufWriter<NamedTempFile<F>>,
    sync: impl FnOnce(&F) -> io::Result<()>,
    dir: &Path,
    destination: &Destination,
    cancelled: impl Fn() -> bool,
) -> Result<PathBuf, JobError> {
    let Destination::Candidates(candidates) = destination else {
        return Err(JobError::Internal(
            "an exact name can't be published as a new file".into(),
        ));
    };
    if cancelled() {
        return Err(JobError::Cancelled);
    }
    // The error holds the writer, and with it the temporary file: both are
    // dropped with it, so the file is deleted.
    let temp = output.into_inner().map_err(|error| {
        io_error(
            error.error(),
            &format!("couldn't write a file in {}", folder(dir)),
        )
    })?;
    sync(temp.as_file())
        .map_err(|error| io_error(&error, &format!("couldn't save a file in {}", folder(dir))))?;
    publish_new(temp, dir, candidates.clone(), cancelled)
}

/// Renames `temp` to the first free name of `candidates` in `dir`.
fn publish_new<F>(
    temp: NamedTempFile<F>,
    dir: &Path,
    candidates: NameCandidates,
    cancelled: impl Fn() -> bool,
) -> Result<PathBuf, JobError> {
    let published = first_free(
        candidates,
        temp,
        |name, temp| {
            let path = dir.join(name);
            match temp.persist_noclobber(&path) {
                Ok(_) => Ok(path),
                Err(PersistError { error, file }) => {
                    if error.kind() == ErrorKind::AlreadyExists {
                        debug!(name = ?name, "name is taken, trying the next one");
                    }
                    Err((error, file))
                }
            }
        },
        cancelled,
    );
    match published {
        Ok(path) => {
            debug!(path = %path.display(), "published");
            Ok(path)
        }
        Err(stop) => Err(stop.into_job_error(dir)),
    }
}

/// Why [`first_free`] stopped without a result.
#[derive(Debug)]
enum Stop {
    /// Cancelled before an attempt.
    Cancelled,
    /// The attempt with `name` failed for another reason than a taken name.
    Failed { name: OsString, error: io::Error },
    /// Every candidate was taken.
    Exhausted,
}

impl Stop {
    fn into_job_error(self, dir: &Path) -> JobError {
        match self {
            Self::Cancelled => JobError::Cancelled,
            Self::Exhausted => JobError::NoFreeName,
            Self::Failed { error, .. } if cannot_rename_safely(&error) => {
                warn!(%error, folder = %dir.display(), "the volume can't rename without replacing");
                JobError::Io {
                    kind: ErrorKind::Unsupported,
                    message: NO_SAFE_PUBLISH.into(),
                }
            }
            Self::Failed { name, error } => io_error(
                &error,
                &format!("couldn't save {} in {}", quoted(&name), folder(dir)),
            ),
        }
    }
}

/// Tries `value` under each candidate in order until an attempt succeeds.
///
/// `attempt` gets `value` back with an error: `AlreadyExists` moves on to
/// the next candidate, any other error stops. Cancellation is checked
/// before every attempt, never after a successful one. On every stop
/// `value` is dropped before this returns.
fn first_free<T, P>(
    candidates: impl IntoIterator<Item = OsString>,
    mut value: T,
    mut attempt: impl FnMut(&OsStr, T) -> Result<P, (io::Error, T)>,
    cancelled: impl Fn() -> bool,
) -> Result<P, Stop> {
    for name in candidates {
        if cancelled() {
            drop(value);
            return Err(Stop::Cancelled);
        }
        match attempt(&name, value) {
            Ok(published) => return Ok(published),
            Err((error, back)) if error.kind() == ErrorKind::AlreadyExists => value = back,
            Err((error, back)) => {
                drop(back);
                return Err(Stop::Failed { name, error });
            }
        }
    }
    drop(value);
    Err(Stop::Exhausted)
}

/// Whether `error` says that the volume has no rename that refuses to
/// replace (and no hard link to fall back on).
///
/// `std` decodes `EOPNOTSUPP` and `ENOSYS` as [`ErrorKind::Unsupported`],
/// but not `ENOTSUP`, which exFAT gives on macOS and which is a different
/// number there (45, not 102).
fn cannot_rename_safely(error: &io::Error) -> bool {
    error.kind() == ErrorKind::Unsupported || is_not_supported_errno(error)
}

#[cfg(unix)]
fn is_not_supported_errno(error: &io::Error) -> bool {
    use rustix::io::Errno;
    Errno::from_io_error(error)
        .is_some_and(|errno| errno == Errno::NOTSUP || errno == Errno::OPNOTSUPP)
}

#[cfg(not(unix))]
fn is_not_supported_errno(_error: &io::Error) -> bool {
    false
}

/// A [`JobError::Io`] that names what failed: `"<action>: <reason>"`.
fn io_error(error: &io::Error, action: &str) -> JobError {
    JobError::Io {
        kind: error.kind(),
        message: format!("{action}: {}", reason(error)),
    }
}

/// The text of `error` up to the ` (os error N)` that `Display` appends.
///
/// That also drops what tempfile appends after it: ` at path "…"` with the
/// name of the temporary file, which means nothing to the user.
fn reason(error: &io::Error) -> String {
    let text = error.to_string();
    match text.split_once(" (os error ") {
        Some((reason, _)) => reason.to_owned(),
        None => text,
    }
}

/// The name of `dir` in quotes, or the whole path for a root.
fn folder(dir: &Path) -> String {
    quoted(dir.file_name().unwrap_or(dir.as_os_str()))
}

fn quoted(name: &OsStr) -> String {
    format!("“{}”", name.to_string_lossy())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::sync::Barrier;
    use std::thread;

    use orbit_domain::operation::{FileName, FileNamePolicy, OperationSpec};
    use tempfile::TempDir;

    use super::*;

    fn zip_of(name: &str) -> Destination {
        OperationSpec::ArchiveZip.destination(OsStr::new(name))
    }

    fn candidates_of(name: &str) -> NameCandidates {
        match zip_of(name) {
            Destination::Candidates(candidates) => candidates,
            Destination::Exact(_) => unreachable!("ZIP gets candidates"),
        }
    }

    /// Names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        names(dir)
            .into_iter()
            .filter(|name| name.starts_with(TEMP_PREFIX))
            .collect()
    }

    fn written(dir: &Path, bytes: &[u8]) -> BufWriter<NamedTempFile> {
        let mut output = BufWriter::new(create_temp(dir).unwrap());
        output.write_all(bytes).unwrap();
        output
    }

    fn taken() -> io::Error {
        io::Error::from(ErrorKind::AlreadyExists)
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn conflicts_move_on_to_the_next_candidate() {
        let tried = RefCell::new(Vec::new());
        let result = first_free(
            candidates_of("Docs"),
            (),
            |name, value| {
                tried.borrow_mut().push(name.to_owned());
                if tried.borrow().len() < 3 {
                    Err((taken(), value))
                } else {
                    Ok(name.to_owned())
                }
            },
            never,
        );
        assert_eq!(result.unwrap(), "Docs 3.zip");
        assert_eq!(tried.into_inner(), ["Docs.zip", "Docs 2.zip", "Docs 3.zip"]);
    }

    #[test]
    fn another_error_stops_with_that_error() {
        let attempts = Cell::new(0);
        let result = first_free(
            candidates_of("Docs"),
            (),
            |_, value| -> Result<(), _> {
                attempts.set(attempts.get() + 1);
                match attempts.get() {
                    1 => Err((taken(), value)),
                    _ => Err((io::Error::from(ErrorKind::PermissionDenied), value)),
                }
            },
            never,
        );
        let Err(Stop::Failed { name, error }) = result else {
            panic!("expected a failure, got {result:?}");
        };
        assert_eq!(name, "Docs 2.zip");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn no_free_name_when_every_candidate_is_taken() {
        let attempts = Cell::new(0_u32);
        let result = first_free(
            candidates_of("Docs"),
            (),
            |_, value| -> Result<(), _> {
                attempts.set(attempts.get() + 1);
                Err((taken(), value))
            },
            never,
        );
        assert!(matches!(result, Err(Stop::Exhausted)));
        assert_eq!(attempts.get(), orbit_domain::operation::MAX_NAME_CANDIDATES);
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            Stop::Exhausted.into_job_error(dir.path()),
            JobError::NoFreeName
        ));
    }

    /// Fails the first attempt with a taken name and the second with
    /// `error`, on a real temporary file; returns the job error.
    fn fail_after_conflict(dir: &Path, error: impl Fn() -> io::Error) -> JobError {
        let attempts = Cell::new(0);
        let result = first_free(
            candidates_of("Docs"),
            create_temp(dir).unwrap(),
            |_, temp| -> Result<(), _> {
                attempts.set(attempts.get() + 1);
                match attempts.get() {
                    1 => Err((taken(), temp)),
                    _ => Err((error(), temp)),
                }
            },
            never,
        );
        result.unwrap_err().into_job_error(dir)
    }

    fn assert_no_safe_publish(error: &JobError) {
        let JobError::Io { kind, message } = error else {
            panic!("expected an I/O error, got {error:?}");
        };
        assert_eq!(*kind, ErrorKind::Unsupported);
        assert_eq!(message, "Orbit can't safely create a ZIP on this disk");
    }

    #[test]
    fn an_unsupported_rename_stops_with_the_disk_message() {
        let dir = TempDir::new().unwrap();
        let error = fail_after_conflict(dir.path(), || io::Error::from(ErrorKind::Unsupported));
        assert_no_safe_publish(&error);
        assert!(leftovers(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn enotsup_stops_with_the_disk_message() {
        use rustix::io::Errno;

        for errno in [Errno::NOTSUP, Errno::OPNOTSUPP] {
            let dir = TempDir::new().unwrap();
            let error = fail_after_conflict(dir.path(), || {
                io::Error::from_raw_os_error(errno.raw_os_error())
            });
            assert_no_safe_publish(&error);
            assert!(leftovers(dir.path()).is_empty());
        }
    }

    #[test]
    fn other_errors_name_the_candidate_and_the_folder() {
        let dir = TempDir::new().unwrap();
        let error = fail_after_conflict(dir.path(), || {
            io::Error::new(ErrorKind::PermissionDenied, "Permission denied")
        });
        let JobError::Io { kind, message } = error else {
            panic!("expected an I/O error, got {error:?}");
        };
        assert_eq!(kind, ErrorKind::PermissionDenied);
        let folder = dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(
            message,
            format!("couldn't save “Docs 2.zip” in “{folder}”: Permission denied")
        );
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn cancelled_before_the_first_attempt() {
        let dir = TempDir::new().unwrap();
        let attempts = Cell::new(0);
        let result = first_free(
            candidates_of("Docs"),
            create_temp(dir.path()).unwrap(),
            |_, temp| -> Result<(), _> {
                attempts.set(attempts.get() + 1);
                Err((taken(), temp))
            },
            || true,
        );
        assert!(matches!(result, Err(Stop::Cancelled)));
        assert_eq!(attempts.get(), 0);
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn cancelled_between_attempts() {
        let dir = TempDir::new().unwrap();
        let cancel = Cell::new(false);
        let attempts = Cell::new(0);
        let result = first_free(
            candidates_of("Docs"),
            create_temp(dir.path()).unwrap(),
            |_, temp| -> Result<(), _> {
                attempts.set(attempts.get() + 1);
                cancel.set(true);
                Err((taken(), temp))
            },
            || cancel.get(),
        );
        assert!(matches!(result, Err(Stop::Cancelled)));
        assert_eq!(attempts.get(), 1);
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn a_cancellation_during_a_successful_rename_is_too_late() {
        let dir = TempDir::new().unwrap();
        let cancel = Cell::new(false);
        let result = first_free(
            candidates_of("Docs"),
            create_temp(dir.path()).unwrap(),
            |name, temp| {
                let path = dir.path().join(name);
                let persisted = temp.persist_noclobber(&path);
                cancel.set(true);
                persisted.map(|_| path).map_err(|e| (e.error, e.file))
            },
            || cancel.get(),
        );
        assert_eq!(result.unwrap(), dir.path().join("Docs.zip"));
        assert_eq!(names(dir.path()), ["Docs.zip"]);
    }

    /// A file whose writes all fail, as on a full disk.
    struct FailingWrite;

    impl Write for FailingWrite {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                ErrorKind::StorageFull,
                "No space left on device",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_flush_publishes_nothing() {
        let dir = TempDir::new().unwrap();
        let (_file, path) = create_temp(dir.path()).unwrap().into_parts();
        let mut output = BufWriter::new(NamedTempFile::from_parts(FailingWrite, path));
        // Fits in the buffer, so only the flush in `commit` writes it.
        output.write_all(b"archive").unwrap();
        let error = commit_with(output, |_| Ok(()), dir.path(), &zip_of("Docs"), never)
            .expect_err("the flush fails");
        let JobError::Io { kind, message } = error else {
            panic!("expected an I/O error, got {error:?}");
        };
        assert_eq!(kind, ErrorKind::StorageFull);
        assert!(
            message.starts_with("couldn't write a file in "),
            "{message}"
        );
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn a_failed_sync_publishes_nothing() {
        let dir = TempDir::new().unwrap();
        let output = written(dir.path(), b"archive");
        let error = commit_with(
            output,
            |_| Err(io::Error::other("sync failed")),
            dir.path(),
            &zip_of("Docs"),
            never,
        )
        .expect_err("the sync fails");
        let JobError::Io { message, .. } = error else {
            panic!("expected an I/O error, got {error:?}");
        };
        assert!(message.starts_with("couldn't save a file in "), "{message}");
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn an_exact_name_is_refused() {
        let dir = TempDir::new().unwrap();
        let name = FileName::new("Docs.zip", &FileNamePolicy::UNIVERSAL).unwrap();
        let output = written(dir.path(), b"archive");
        let error = commit(output, dir.path(), &Destination::Exact(name), never)
            .expect_err("Rename is not published as a new file");
        assert!(matches!(error, JobError::Internal(_)), "{error:?}");
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn a_cancelled_commit_publishes_nothing() {
        let dir = TempDir::new().unwrap();
        let output = written(dir.path(), b"archive");
        let error = commit(output, dir.path(), &zip_of("Docs"), || true).unwrap_err();
        assert!(matches!(error, JobError::Cancelled), "{error:?}");
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn a_free_name_gets_the_file() {
        let dir = TempDir::new().unwrap();
        let path = commit(
            written(dir.path(), b"new"),
            dir.path(),
            &zip_of("Docs"),
            never,
        )
        .unwrap();
        assert_eq!(path, dir.path().join("Docs.zip"));
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(names(dir.path()), ["Docs.zip"]);
    }

    /// Publishes `new` next to whatever `occupy` put at `Docs.zip`.
    fn publish_next_to(occupy: impl FnOnce(&Path)) -> TempDir {
        let dir = TempDir::new().unwrap();
        occupy(&dir.path().join("Docs.zip"));
        let path = commit(
            written(dir.path(), b"new"),
            dir.path(),
            &zip_of("Docs"),
            never,
        )
        .unwrap();
        assert_eq!(path, dir.path().join("Docs 2.zip"));
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(names(dir.path()), ["Docs 2.zip", "Docs.zip"]);
        dir
    }

    #[test]
    fn a_taken_file_name_is_left_alone() {
        let dir = publish_next_to(|taken| fs::write(taken, b"old").unwrap());
        assert_eq!(fs::read(dir.path().join("Docs.zip")).unwrap(), b"old");
    }

    // On Windows a folder in the way is not checked yet.
    #[cfg(unix)]
    #[test]
    fn a_folder_in_the_way_is_left_alone() {
        let dir = publish_next_to(|taken| {
            fs::create_dir(taken).unwrap();
            fs::write(taken.join("inside"), b"old").unwrap();
        });
        assert_eq!(
            fs::read(dir.path().join("Docs.zip/inside")).unwrap(),
            b"old"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_in_the_way_is_left_alone() {
        let dir = publish_next_to(|taken| std::os::unix::fs::symlink("missing", taken).unwrap());
        let link = dir.path().join("Docs.zip");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("missing"));
        assert!(!dir.path().join("missing").exists());
    }

    // The limit of 255 bytes per name is not the same on Windows.
    #[cfg(unix)]
    #[test]
    fn a_name_too_long_stops_without_touching_the_first() {
        let dir = TempDir::new().unwrap();
        let base = "a".repeat(251);
        let first = dir.path().join(format!("{base}.zip"));
        fs::write(&first, b"old").unwrap();
        let error = commit(
            written(dir.path(), b"new"),
            dir.path(),
            &zip_of(&base),
            never,
        )
        .expect_err("the second candidate is 257 bytes");
        let JobError::Io { kind, .. } = error else {
            panic!("expected an I/O error, got {error:?}");
        };
        assert_eq!(kind, ErrorKind::InvalidFilename);
        assert_eq!(fs::read(&first).unwrap(), b"old");
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn two_jobs_on_one_name_get_two_names() {
        let dir = TempDir::new().unwrap();
        let start = Barrier::new(2);
        let mut published: Vec<(PathBuf, &[u8])> = thread::scope(|scope| {
            let jobs: Vec<_> = [b"first".as_slice(), b"second"]
                .into_iter()
                .map(|bytes| {
                    let (dir, start) = (dir.path(), &start);
                    scope.spawn(move || {
                        let output = written(dir, bytes);
                        start.wait();
                        (commit(output, dir, &zip_of("Docs"), never).unwrap(), bytes)
                    })
                })
                .collect();
            jobs.into_iter().map(|job| job.join().unwrap()).collect()
        });
        published.sort();
        let paths: Vec<_> = published.iter().map(|(path, _)| path.clone()).collect();
        assert_eq!(
            paths,
            [dir.path().join("Docs 2.zip"), dir.path().join("Docs.zip")]
        );
        for (path, bytes) in published {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert!(leftovers(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn temporary_files_are_hidden_and_published_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new().unwrap();
        let output = written(dir.path(), b"new");
        let name = output
            .get_ref()
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with(TEMP_PREFIX) && name.ends_with(TEMP_SUFFIX),
            "{name}"
        );
        let path = commit(output, dir.path(), &zip_of("Docs"), never).unwrap();
        // 0o666 less the umask; tempfile alone would give 0o600.
        let mode = fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o044, 0o044, "mode {mode:o}");
    }

    #[test]
    fn a_missing_folder_is_named_in_the_error() {
        let error = create_temp(Path::new("/nonexistent/Orbit")).unwrap_err();
        let JobError::Io { kind, message } = error else {
            panic!("expected an I/O error, got {error:?}");
        };
        assert_eq!(kind, ErrorKind::NotFound);
        assert!(
            message.starts_with("couldn't create a file in “Orbit”: "),
            "{message}"
        );
        assert!(!message.contains("os error"), "{message}");
    }

    #[test]
    fn reasons_drop_the_os_error_number() {
        let error = io::Error::from_raw_os_error(2);
        assert!(!reason(&error).contains("os error"), "{}", reason(&error));
        assert_eq!(reason(&io::Error::other("custom")), "custom");
        let wrapped = io::Error::other(format!("{error} at path \"/tmp/.orbit-1.tmp\""));
        assert_eq!(reason(&wrapped), reason(&error));
    }

    #[test]
    fn a_root_folder_is_named_by_its_path() {
        assert_eq!(folder(Path::new("/")), "“/”");
        assert_eq!(folder(Path::new("/Users/me/Docs")), "“Docs”");
    }
}
