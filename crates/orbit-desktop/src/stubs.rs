//! Stand-ins for the ports that later stages implement: the Finder resolver
//! (stage 4) and the operations (stage 7).

use orbit_app::messages::{JobError, JobOutput, JobSpec, ResolveError};
use orbit_app::ports::{JobContext, OperationExecutor, ResolveRequest, TargetResolver};
use orbit_domain::target::FileTarget;

/// Refuses every gesture.
#[derive(Debug, Default)]
pub struct UnavailableResolver;

impl TargetResolver for UnavailableResolver {
    fn resolve(&mut self, _: &ResolveRequest) -> Result<FileTarget, ResolveError> {
        Err(ResolveError::Failed(
            "target resolution is not implemented yet".into(),
        ))
    }
}

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
