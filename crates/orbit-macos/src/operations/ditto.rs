//! ZIP archives made by `/usr/bin/ditto`, for development and diagnosis.
//!
//! # What is read
//!
//! ditto opens the source by its path and follows a symbolic link given as
//! the source, so this backend can't promise what the portable one does.
//! The source is checked against the target before ditto starts and again
//! after it ends, before the archive is published: the identity and the
//! type, and for a file also its size and modification time. A source that
//! is swapped and swapped back while ditto runs is not noticed.
//!
//! # Output
//!
//! ditto writes over the temporary file that Orbit created, which is then
//! published like every result, never over another file. A cancelled job
//! kills ditto; its half-written file is deleted. Progress stays
//! indeterminate.

use std::fs;
use std::io::{BufWriter, ErrorKind, Read};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use orbit_app::messages::{JobError, JobOutput, JobSpec};
use orbit_app::ports::JobContext;
use orbit_domain::target::{FileIdentity, FileKind, FileTarget, FileVersion, SourceChanged};
use orbit_ops::{archive, publish};
use tracing::{info, warn};

use super::children::Children;

const DITTO: &str = "/usr/bin/ditto";
/// How often a running child is checked for its end and for cancellation.
const POLL: Duration = Duration::from_millis(50);
/// How much of the end of stderr is kept for the error message.
const STDERR_TAIL: usize = 4096;

/// Creates a ZIP archive of the target of `spec` next to it with ditto.
///
/// # Errors
///
/// - the errors of [`archive::check`] for a job this backend can't run;
/// - [`JobError::SourceChanged`] when the source differs from the target
///   before ditto starts or after it ends;
/// - [`JobError::Cancelled`] when `context` is cancelled;
/// - [`JobError::Io`] when ditto can't be started or fails, with the last
///   line ditto wrote to stderr;
/// - the errors of [`publish::commit`].
pub fn archive_zip(
    spec: &JobSpec,
    context: &JobContext,
    children: &Children,
) -> Result<JobOutput, JobError> {
    archive::check(spec)?;
    let target = &spec.target;
    let Some(dir) = target.path.parent() else {
        return Err(JobError::Internal("the target has no folder".into()));
    };
    check_source(target, true)?;
    let temp = publish::create_temp(dir)?;
    let mut command = Command::new(DITTO);
    command.args(["-c", "-k", "--sequesterRsrc"]);
    // A folder goes in under its own name; for a file the flag would put
    // the name of its parent folder above it.
    if target.facts.kind != FileKind::File {
        command.arg("--keepParent");
    }
    command.arg(&target.path).arg(temp.path());
    run(command, context, children)?;
    // A folder changes its modification time when its entries change, which
    // the portable backend allows too; a file must be as it was.
    check_source(target, target.facts.kind == FileKind::File)?;
    let path = publish::commit(BufWriter::new(temp), dir, &spec.destination, || {
        context.is_cancelled()
    })?;
    info!(path = %path.display(), "zip archive created by ditto");
    Ok(JobOutput { path })
}

/// Checks that the object at the path of `target` is still the one it was
/// read from: same identity and type, and same size and modification time
/// when `with_version` is set.
fn check_source(target: &FileTarget, with_version: bool) -> Result<(), JobError> {
    let metadata = match fs::symlink_metadata(&target.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(SourceChanged::Replaced.into());
        }
        Err(error) => {
            let name = target.path.file_name().unwrap_or_default().display();
            return Err(JobError::Io {
                kind: error.kind(),
                message: format!("couldn't read “{name}”: {error}"),
            });
        }
    };
    let same_type = match target.facts.kind {
        FileKind::File => metadata.is_file(),
        FileKind::Directory | FileKind::Package => metadata.is_dir(),
        FileKind::Symlink => false,
    };
    let identity = FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    if !same_type || identity != target.identity {
        return Err(SourceChanged::Replaced.into());
    }
    if with_version {
        let version = FileVersion {
            size: metadata.len(),
            modified: metadata.modified().map_err(|error| JobError::Io {
                kind: error.kind(),
                message: format!(
                    "couldn't read the time of “{}”: {error}",
                    target.path.display()
                ),
            })?,
        };
        target.check_source(identity, version)?;
    }
    Ok(())
}

/// Runs `command` to its end, or kills it when `context` is cancelled.
///
/// stdin and stdout are closed; stderr is read while the child runs, so a
/// full pipe never stops it, and only its end is kept.
fn run(mut command: Command, context: &JobContext, children: &Children) -> Result<(), JobError> {
    if context.is_cancelled() {
        return Err(JobError::Cancelled);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| JobError::Io {
        kind: error.kind(),
        message: format!("couldn't start {}: {error}", program(&command)),
    })?;
    let stderr = child.stderr.take();
    let mut child = children.track(child);
    let (outcome, tail) = thread::scope(|scope| {
        let reader = scope.spawn(move || stderr.map(read_tail).unwrap_or_default());
        // Every way out of `wait` reaps the child, which closes the pipe and
        // ends the reader.
        let outcome = wait(&mut child, context);
        (outcome, reader.join().unwrap_or_default())
    });
    let status = outcome?;
    if status.success() {
        return Ok(());
    }
    warn!(%status, stderr = %tail, "{} failed", program(&command));
    let message = tail
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or_else(
            || format!("{} failed ({status})", program(&command)),
            str::to_owned,
        );
    Err(JobError::Io {
        kind: ErrorKind::Other,
        message,
    })
}

/// Waits for the child to end, checking for cancellation every [`POLL`].
fn wait(
    child: &mut super::children::Tracked<'_>,
    context: &JobContext,
) -> Result<ExitStatus, JobError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                return Err(JobError::Io {
                    kind: error.kind(),
                    message: format!("couldn't wait for a child process: {error}"),
                });
            }
        }
        if context.is_cancelled() {
            if let Err(error) = child.kill() {
                warn!(%error, "couldn't reap a cancelled child process");
            }
            return Err(JobError::Cancelled);
        }
        thread::sleep(POLL);
    }
}

/// Reads `source` to its end and keeps the last [`STDERR_TAIL`] bytes.
fn read_tail(mut source: impl Read) -> String {
    let mut tail = Vec::new();
    let mut block = [0_u8; 8192];
    loop {
        match source.read(&mut block) {
            Ok(0) => break,
            Ok(read) => tail.extend_from_slice(block.get(..read).unwrap_or_default()),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                warn!(%error, "couldn't read the stderr of a child process");
                break;
            }
        }
        if tail.len() > 2 * STDERR_TAIL {
            tail.drain(..tail.len() - STDERR_TAIL);
        }
    }
    if tail.len() > STDERR_TAIL {
        tail.drain(..tail.len() - STDERR_TAIL);
    }
    String::from_utf8_lossy(&tail).into_owned()
}

fn program(command: &Command) -> String {
    Path::new(command.get_program())
        .file_name()
        .unwrap_or_default()
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Instant;

    use orbit_app::ports::CancelToken;

    use super::*;
    use crate::operations::test_support::{context, leftovers, names, ok, spec, target, tempdir};

    #[test]
    fn ditto_archives_a_folder_under_a_free_name() {
        let dir = tempdir();
        let docs = dir.path().join("Docs");
        ok(fs::create_dir(&docs), "mkdir");
        ok(fs::write(docs.join("a.txt"), b"alpha"), "write");
        ok(fs::write(dir.path().join("Docs.zip"), b"taken"), "write");
        let children = Children::new();
        let output = archive_zip(
            &spec(target(&docs)),
            &context(&CancelToken::new()),
            &children,
        );
        let path = dir.path().join("Docs 2.zip");
        assert_eq!(output, Ok(JobOutput { path: path.clone() }));
        let names = names(&path);
        assert!(names.iter().any(|name| name == "Docs/a.txt"), "{names:?}");
        assert_eq!(ok(fs::read(dir.path().join("Docs.zip")), "read"), b"taken");
        assert!(leftovers(dir.path()).is_empty());
        assert!(children.is_empty());
    }

    #[test]
    fn ditto_failure_is_told_with_its_last_line() {
        if ok(fs::metadata("/"), "stat /").uid() == 0 {
            return;
        }
        let dir = tempdir();
        let docs = dir.path().join("Docs");
        ok(fs::create_dir(&docs), "mkdir");
        let secret = docs.join("secret.txt");
        ok(fs::write(&secret, b"no"), "write");
        let target = target(&docs);
        ok(
            fs::set_permissions(&secret, Permissions::from_mode(0o000)),
            "chmod",
        );
        let output = archive_zip(
            &spec(target),
            &context(&CancelToken::new()),
            &Children::new(),
        );
        let Err(JobError::Io { kind, message }) = output else {
            panic!("{output:?}");
        };
        assert_eq!(kind, ErrorKind::Other);
        assert!(message.starts_with("ditto: "), "{message}");
        assert!(message.contains("Permission denied"), "{message}");
        assert!(leftovers(dir.path()).is_empty());
        assert!(!dir.path().join("Docs.zip").exists());
    }

    #[test]
    fn a_source_replaced_before_the_start_is_not_archived() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        ok(fs::write(&file, b"alpha"), "write");
        let target = target(&file);
        ok(fs::remove_file(&file), "remove");
        ok(fs::write(&file, b"other"), "write");
        let output = archive_zip(
            &spec(target),
            &context(&CancelToken::new()),
            &Children::new(),
        );
        assert_eq!(output, Err(SourceChanged::Replaced.into()));
        assert!(leftovers(dir.path()).is_empty());
    }

    /// After ditto, a file must be as it was; a folder may have new entries.
    #[test]
    fn the_check_after_ditto_lets_a_folder_change() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        ok(fs::write(&file, b"alpha"), "write");
        let docs = dir.path().join("Docs");
        ok(fs::create_dir(&docs), "mkdir");
        let file_target = target(&file);
        let folder_target = target(&docs);
        ok(fs::write(&file, b"alpha and more"), "write");
        ok(fs::write(docs.join("new.txt"), b"new"), "write");
        assert_eq!(
            check_source(&file_target, true),
            Err(SourceChanged::MetadataChanged.into())
        );
        assert_eq!(check_source(&folder_target, false), Ok(()));
        ok(fs::remove_dir_all(&docs), "remove");
        ok(fs::write(&docs, b"now a file"), "write");
        assert_eq!(
            check_source(&folder_target, false),
            Err(SourceChanged::Replaced.into())
        );
    }

    #[test]
    fn a_cancelled_child_is_killed_and_reaped() {
        let cancel = CancelToken::new();
        let children = Children::new();
        let canceller = {
            let cancel = cancel.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(150));
                cancel.cancel();
            })
        };
        let start = Instant::now();
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let outcome = run(command, &context(&cancel), &children);
        assert_eq!(outcome, Err(JobError::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(children.is_empty());
        let _ = canceller.join();
    }

    #[test]
    fn a_full_stderr_does_not_stop_the_child_and_its_end_is_kept() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("yes noise | head -c 400000 >&2; echo >&2; echo 'ditto: x: Permission denied' >&2; exit 3");
        let outcome = run(command, &context(&CancelToken::new()), &Children::new());
        assert_eq!(
            outcome,
            Err(JobError::Io {
                kind: ErrorKind::Other,
                message: "ditto: x: Permission denied".into(),
            })
        );
    }

    #[test]
    fn a_silent_failure_names_the_program_and_the_status() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exit 2");
        let outcome = run(command, &context(&CancelToken::new()), &Children::new());
        let Err(JobError::Io { message, .. }) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(message.starts_with("sh failed ("), "{message}");
    }

    #[test]
    fn stderr_keeps_only_its_end() {
        let text = "x".repeat(10_000) + "\nlast line\n";
        let tail = read_tail(text.as_bytes());
        assert_eq!(tail.len(), STDERR_TAIL);
        assert!(tail.ends_with("last line\n"));
    }
}
