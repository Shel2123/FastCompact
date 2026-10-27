//! Stand-ins for the ports that later stages implement: the operations
//! (stage 7).

use orbit_app::messages::{JobError, JobOutput, JobSpec};
use orbit_app::ports::{JobContext, OperationExecutor};

/// Fails every job.
#[derive(Debug, Default)]
pub struct UnavailableExecutor;

impl OperationExecutor for UnavailableExecutor {
    fn execute(&self, _: &JobSpec, _: &JobContext) -> Result<JobOutput, JobError> {
        Err(JobError::Internal(
            "operations are not implemented yet".into(),
        ))
    }
}
