//! Contracts for external capabilities.
//!
//! The runtime implements them with threads and macOS adapters; tests
//! implement them with fakes.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use orbit_domain::target::FileTarget;

use crate::messages::{AppEvent, GestureId, JobError, JobOutput, JobSpec, Progress, ResolveError};

/// Delivers events to the controller on the main thread.
///
/// # Contract
///
/// 1. If `send(a)` returned before `send(b)` started, `a` is delivered
///    before `b`, whichever threads the two calls came from. Calls that
///    overlap have no order; the controller tells their events apart by id.
/// 2. `send` never runs the handler synchronously, so the controller is
///    never entered again from inside its own update.
/// 3. `send` does not block and loses nothing while the application runs.
///    Events sent after it has stopped are dropped.
pub trait EventSink: Send + Sync {
    /// Queues `event` for the controller.
    fn send(&self, event: AppEvent);
}

/// A request to read the target of a gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveRequest {
    /// The press; the answer carries it.
    pub gesture: GestureId,
    /// When the key went down, for the selection guard.
    pub pressed_at: Instant,
}

/// Reads the target of a gesture.
///
/// Not `Send`: an implementation may hold handles that must stay on one
/// thread. The runtime creates it, calls it and drops it on its own thread.
/// State kept between calls must be a cache only: after a panic in
/// [`resolve`](Self::resolve) the resolver is called again.
pub trait TargetResolver {
    /// Reads the target, blocking the calling thread.
    fn resolve(&mut self, request: &ResolveRequest) -> Result<FileTarget, ResolveError>;
}

/// Runs a job on a worker thread.
pub trait OperationExecutor: Send + Sync {
    /// Runs `spec`, blocking the calling thread. Checks
    /// [`JobContext::is_cancelled`] often enough to stop soon after a
    /// cancellation.
    fn execute(&self, spec: &JobSpec, context: &JobContext) -> Result<JobOutput, JobError>;
}

/// Shared flag that asks a job to stop.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the job to stop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether the job was asked to stop.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What a running job can use, lent to [`OperationExecutor::execute`].
///
/// It is lent, not cloned, so every [`report`](Self::report) happens before
/// `execute` returns, even from scoped threads. By rule 1 of [`EventSink`]
/// the progress is then delivered before the job's
/// [`JobFinished`](AppEvent::JobFinished).
pub struct JobContext {
    cancel: CancelToken,
    progress: Box<dyn Fn(Progress) + Send + Sync>,
}

impl JobContext {
    /// A context that reads `cancel` and passes progress to `progress`.
    pub fn new(cancel: CancelToken, progress: impl Fn(Progress) + Send + Sync + 'static) -> Self {
        Self {
            cancel,
            progress: Box::new(progress),
        }
    }

    /// Whether the job was asked to stop.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Reports progress. Values may be dropped to limit the event rate.
    pub fn report(&self, progress: Progress) {
        (self.progress)(progress);
    }
}

impl fmt::Debug for JobContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobContext")
            .field("cancel", &self.cancel)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn cancel_token_is_shared() {
        let token = CancelToken::new();
        let copy = token.clone();
        assert!(!copy.is_cancelled());
        token.cancel();
        assert!(copy.is_cancelled());
    }

    #[test]
    fn context_reports_and_reads_cancel() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let token = CancelToken::new();
        let context = JobContext::new(token.clone(), move |progress| {
            if let Ok(mut seen) = sink.lock() {
                seen.push(progress);
            }
        });
        context.report(Progress::Indeterminate);
        assert!(!context.is_cancelled());
        token.cancel();
        assert!(context.is_cancelled());
        let seen = seen.lock().map(|seen| seen.clone()).unwrap_or_default();
        assert_eq!(seen, vec![Progress::Indeterminate]);
    }
}
