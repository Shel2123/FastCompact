//! ZIP archives of a file or a folder.
//!
//! # What is read
//!
//! Only the object the user chose and what lies inside it. After the root,
//! nothing is opened by a full path: every entry is opened relative to the
//! descriptor of its folder, without following symbolic links, and checked
//! to be the object that was listed. A symbolic link is stored as a link and
//! never followed; FIFOs, sockets and devices are skipped without being
//! opened; entries on another volume are skipped.
//!
//! The chosen object must still be the one of the target (identity, size
//! and modification time) when the job starts, or the job fails with
//! [`SourceChanged`](orbit_domain::target::SourceChanged). A chosen file
//! that changes while it is read fails the same way. Inside a folder,
//! entries added or removed during the job end up in the archive as the
//! writing pass saw them, like in Finder; an entry that is replaced between
//! listing and opening fails the job.
//!
//! # Publishing
//!
//! The archive is written to a temporary file in the folder of the target
//! and published under the first free name with [`commit`](crate::publish::commit);
//! a taken name never makes Orbit compress again.

use std::fs::File;
use std::io;
use std::time::SystemTime;

use orbit_app::messages::{JobError, JobOutput, JobSpec};
use orbit_app::ports::JobContext;
use orbit_domain::action::ActionId;
use orbit_domain::operation::{Destination, OperationSpec};

mod time;

#[cfg(unix)]
mod entry;
#[cfg(unix)]
mod tree;
#[cfg(unix)]
mod writer;

pub use time::LocalTime;

/// Reads the metadata of an open file or folder that only the platform
/// knows how to store, such as the extended attributes of macOS in
/// AppleDouble form.
///
/// `Ok(None)` means there is nothing to store. An error fails the job.
pub type Sidecar<'a> = &'a mut dyn FnMut(&File) -> io::Result<Option<Vec<u8>>>;

/// What the platform adds to a portable archive.
pub struct ArchiveOptions<'a> {
    /// Converts a modification time to local time for the DOS time field,
    /// which has no time zone. `None` stores 1980-01-01 00:00:00. The exact
    /// time in UTC is stored separately in an extended timestamp field.
    pub local_time: fn(SystemTime) -> Option<LocalTime>,
    /// Called for every regular file and folder with the descriptor the walk
    /// has checked. Bytes it returns are stored as
    /// `__MACOSX/<folder>/._<name>` right after the object, the way macOS
    /// tools store AppleDouble files; Archive Utility and `ditto` restore
    /// them. `None` stores nothing.
    pub sidecar: Option<Sidecar<'a>>,
}

/// Creates a ZIP archive of the target of `spec` next to it.
///
/// Reports [`Progress::Indeterminate`](orbit_app::messages::Progress) while
/// it measures the tree, then the fraction of source bytes read. Stops with
/// [`JobError::Cancelled`] soon after `context` is cancelled, leaving
/// nothing behind.
///
/// # Errors
///
/// - [`JobError::Internal`] when `spec` is not a ZIP job for an available
///   target with name candidates, or on a platform without the safe walk;
/// - [`JobError::SourceChanged`] when the chosen object was replaced or
///   modified, or an entry inside it was replaced while the job ran;
/// - [`JobError::Io`] when something cannot be read or written; the message
///   names the entry or the folder;
/// - the errors of [`commit`](crate::publish::commit).
pub fn archive_zip(
    spec: &JobSpec,
    context: &JobContext,
    options: ArchiveOptions<'_>,
) -> Result<JobOutput, JobError> {
    check(spec)?;
    #[cfg(unix)]
    {
        writer::archive(spec, context, options, &mut |_| {})
    }
    #[cfg(not(unix))]
    {
        let _ = (context, options);
        Err(JobError::Internal(
            "ZIP archives aren't supported on this platform yet".into(),
        ))
    }
}

/// Checks that `spec` is a job [`archive_zip`] can run, before any work.
///
/// The controller only starts ZIP jobs for available targets with name
/// candidates, so anything else is a fault of Orbit. A platform backend that
/// archives another way checks its jobs with this too.
///
/// # Errors
///
/// [`JobError::Internal`] when `spec` is not a ZIP job, its target can't be
/// archived or its destination is not a list of name candidates.
pub fn check(spec: &JobSpec) -> Result<(), JobError> {
    if spec.operation != OperationSpec::ArchiveZip {
        return Err(JobError::Internal(format!(
            "{:?} is not a ZIP job",
            spec.operation
        )));
    }
    if !ActionId::ArchiveZip
        .availability(&spec.target.facts)
        .is_available()
    {
        return Err(JobError::Internal(format!(
            "a {:?} can't be archived",
            spec.target.facts.kind
        )));
    }
    if !matches!(spec.destination, Destination::Candidates(_)) {
        return Err(JobError::Internal(
            "a ZIP archive needs name candidates".into(),
        ));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
