//! The operations of macOS: the executor of jobs and its backends.

use std::thread;
use std::time::{Duration, Instant};

use orbit_app::messages::{JobError, JobOutput, JobSpec};
use orbit_app::ports::{JobContext, OperationExecutor};
use orbit_domain::operation::OperationSpec;
use orbit_ops::archive::{self, ArchiveOptions};
use tracing::warn;

pub mod apple_double;
pub mod children;
pub mod ditto;
pub mod local_time;

#[cfg(test)]
mod test_support;

use apple_double::AppleDouble;
use children::Children;

/// How often a debug delay checks for cancellation.
const DELAY_POLL: Duration = Duration::from_millis(50);

/// What makes ZIP archives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ZipBackend {
    /// The portable archiver of `orbit-ops` with the extended attributes of
    /// macOS in AppleDouble form: progress, a safe walk of the tree.
    #[default]
    Portable,
    /// `/usr/bin/ditto`, for development and diagnosis; see [`ditto`].
    Ditto,
}

impl ZipBackend {
    /// `"portable"` or `"ditto"`; `None` for anything else.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "portable" => Some(Self::Portable),
            "ditto" => Some(Self::Ditto),
            _ => None,
        }
    }
}

/// Pauses before every job, for the manual checks of stage 7.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobPauses {
    /// The job waits this long before it starts, and stops early when it is
    /// cancelled, to let a source be replaced meanwhile.
    pub delay: Duration,
    /// The worker sleeps this long before the job without looking at
    /// cancellation, like a worker stuck in a system call.
    pub stall: Duration,
}

/// Runs the jobs of macOS.
///
/// Only ZIP archives for now; the other operations are left out of the
/// supported actions of the platform, so they never reach it.
#[derive(Debug, Clone)]
pub struct MacExecutor {
    zip: ZipBackend,
    children: Children,
    pauses: JobPauses,
}

impl MacExecutor {
    /// An executor that makes ZIP archives with `zip` and keeps the child
    /// processes of its jobs in `children`.
    pub fn new(zip: ZipBackend, children: Children, pauses: JobPauses) -> Self {
        Self {
            zip,
            children,
            pauses,
        }
    }

    fn pause(&self, context: &JobContext) -> Result<(), JobError> {
        let JobPauses { delay, stall } = self.pauses;
        if !stall.is_zero() {
            warn!(?stall, "debug: the worker stalls");
            thread::sleep(stall);
        }
        if delay.is_zero() {
            return Ok(());
        }
        warn!(?delay, "debug: the job waits");
        let deadline = Instant::now() + delay;
        loop {
            if context.is_cancelled() {
                return Err(JobError::Cancelled);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            thread::sleep(left.min(DELAY_POLL));
        }
    }

    fn archive_zip(&self, spec: &JobSpec, context: &JobContext) -> Result<JobOutput, JobError> {
        match self.zip {
            ZipBackend::Portable => {
                let mut packer = AppleDouble::new();
                let mut sidecar = |file: &std::fs::File| packer.pack(file);
                archive::archive_zip(
                    spec,
                    context,
                    ArchiveOptions {
                        local_time: local_time::local_time,
                        sidecar: Some(&mut sidecar),
                    },
                )
            }
            ZipBackend::Ditto => ditto::archive_zip(spec, context, &self.children),
        }
    }
}

impl OperationExecutor for MacExecutor {
    fn execute(&self, spec: &JobSpec, context: &JobContext) -> Result<JobOutput, JobError> {
        self.pause(context)?;
        match spec.operation {
            OperationSpec::ArchiveZip => self.archive_zip(spec, context),
            OperationSpec::ConvertImage { .. }
            | OperationSpec::ResizeImage { .. }
            | OperationSpec::Rename { .. } => Err(JobError::Internal(format!(
                "{:?} is not supported on macOS yet",
                spec.operation
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use orbit_app::ports::CancelToken;
    use orbit_domain::operation::OutputFormat;

    use super::*;
    use crate::operations::test_support::{context, leftovers, names, ok, spec, target, tempdir};

    fn executor(zip: ZipBackend, pauses: JobPauses) -> MacExecutor {
        MacExecutor::new(zip, Children::new(), pauses)
    }

    fn run(command: &mut Command) {
        let status = ok(command.status(), "command");
        assert!(status.success(), "{command:?}: {status}");
    }

    #[test]
    fn backend_names() {
        assert_eq!(ZipBackend::parse("portable"), Some(ZipBackend::Portable));
        assert_eq!(ZipBackend::parse("ditto"), Some(ZipBackend::Ditto));
        assert_eq!(ZipBackend::parse("Ditto"), None);
        assert_eq!(ZipBackend::parse(""), None);
    }

    /// An extended attribute survives the portable archive, and the
    /// AppleDouble entries sit where ditto puts them.
    #[test]
    fn portable_zip_keeps_extended_attributes() {
        let dir = tempdir();
        let docs = dir.path().join("Docs");
        ok(fs::create_dir(&docs), "mkdir");
        let file = docs.join("a.txt");
        ok(fs::write(&file, b"alpha"), "write");
        run(Command::new("xattr")
            .args(["-w", "org.orbit.test", "kept"])
            .arg(&file));
        let output = executor(ZipBackend::Portable, JobPauses::default())
            .execute(&spec(target(&docs)), &context(&CancelToken::new()));
        let archive = dir.path().join("Docs.zip");
        assert_eq!(
            output,
            Ok(JobOutput {
                path: archive.clone()
            })
        );
        let names = names(&archive);
        assert!(
            names.iter().any(|name| name == "__MACOSX/Docs/._a.txt"),
            "{names:?}"
        );

        let out = dir.path().join("out");
        run(Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(&archive)
            .arg(&out));
        let restored = ok(
            Command::new("xattr")
                .args(["-p", "org.orbit.test"])
                .arg(out.join("Docs/a.txt"))
                .output(),
            "xattr -p",
        );
        assert_eq!(String::from_utf8_lossy(&restored.stdout).trim(), "kept");
        assert_eq!(ok(fs::read(out.join("Docs/a.txt")), "read"), b"alpha");
    }

    #[test]
    fn apple_double_packs_an_open_file_with_attributes() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        ok(fs::write(&file, b"alpha"), "write");
        run(Command::new("xattr")
            .args(["-w", "org.orbit.test", "kept"])
            .arg(&file));
        let open = ok(fs::File::open(&file), "open");
        let mut packer = AppleDouble::new();
        let first = ok(packer.pack(&open), "pack");
        let second = ok(packer.pack(&open), "pack again");
        let Some(bytes) = first else {
            panic!("no sidecar");
        };
        // AppleDouble magic number, and the scratch file is reused cleanly.
        assert_eq!(bytes.get(..4), Some(&[0x00, 0x05, 0x16, 0x07][..]));
        assert!(bytes.windows(14).any(|window| window == b"org.orbit.test"));
        assert_eq!(second, Some(bytes));
    }

    #[test]
    fn ditto_backend_is_chosen() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        ok(fs::write(&file, b"alpha"), "write");
        let output = executor(ZipBackend::Ditto, JobPauses::default())
            .execute(&spec(target(&file)), &context(&CancelToken::new()));
        let archive = dir.path().join("a.txt.zip");
        assert_eq!(
            output,
            Ok(JobOutput {
                path: archive.clone()
            })
        );
        let names = names(&archive);
        assert!(names.iter().any(|name| name == "a.txt"), "{names:?}");
        assert!(
            names
                .iter()
                .all(|name| !name.contains('/') || name.starts_with("__MACOSX/")),
            "{names:?}"
        );
    }

    #[test]
    fn other_operations_are_refused_before_any_work() {
        let dir = tempdir();
        let file = dir.path().join("a.png");
        ok(fs::write(&file, b"png"), "write");
        let mut job = spec(target(&file));
        job.operation = OperationSpec::ConvertImage {
            format: OutputFormat::Png,
        };
        let output = executor(ZipBackend::Portable, JobPauses::default())
            .execute(&job, &context(&CancelToken::new()));
        assert!(matches!(output, Err(JobError::Internal(_))), "{output:?}");
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn a_debug_delay_ends_early_on_cancel() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        ok(fs::write(&file, b"alpha"), "write");
        let cancel = CancelToken::new();
        let pauses = JobPauses {
            delay: Duration::from_secs(30),
            stall: Duration::ZERO,
        };
        let canceller = {
            let cancel = cancel.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                cancel.cancel();
            })
        };
        let start = Instant::now();
        let output =
            executor(ZipBackend::Portable, pauses).execute(&spec(target(&file)), &context(&cancel));
        assert_eq!(output, Err(JobError::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!dir.path().join("a.txt.zip").exists());
        let _ = canceller.join();
    }
}
