//! An executor with only the portable operations.

use std::time::SystemTime;

use orbit_app::messages::{JobError, JobOutput, JobSpec};
use orbit_app::ports::{JobContext, OperationExecutor};
use orbit_domain::operation::OperationSpec;

use crate::archive::{self, ArchiveOptions, LocalTime};

/// Runs ZIP jobs with the portable archiver and no platform metadata.
///
/// A platform executor that adds its own metadata calls
/// [`archive::archive_zip`] itself.
#[derive(Debug, Clone, Copy)]
pub struct PortableExecutor {
    local_time: fn(SystemTime) -> Option<LocalTime>,
}

impl PortableExecutor {
    /// An executor that stores DOS times converted by `local_time`.
    pub const fn new(local_time: fn(SystemTime) -> Option<LocalTime>) -> Self {
        Self { local_time }
    }
}

impl OperationExecutor for PortableExecutor {
    fn execute(&self, spec: &JobSpec, context: &JobContext) -> Result<JobOutput, JobError> {
        match spec.operation {
            OperationSpec::ArchiveZip => archive::archive_zip(
                spec,
                context,
                ArchiveOptions {
                    local_time: self.local_time,
                    sidecar: None,
                },
            ),
            OperationSpec::ConvertImage { .. }
            | OperationSpec::ResizeImage { .. }
            | OperationSpec::Rename { .. } => Err(JobError::Internal(format!(
                "{:?} has no portable implementation",
                spec.operation
            ))),
        }
    }
}
