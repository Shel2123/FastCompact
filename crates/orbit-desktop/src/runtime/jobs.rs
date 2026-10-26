//! Job queue and worker pool.

use std::collections::HashMap;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use orbit_app::messages::{AppEvent, JobError, JobId, JobOutput, JobSpec};
use orbit_app::ports::{CancelToken, EventSink, JobContext, OperationExecutor};
use tracing::{debug, error, info, warn};

/// Shortest time between two progress events of one job.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Size of the worker pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobsConfig {
    /// Worker threads; at least 1.
    pub workers: usize,
    /// Jobs that may wait for a worker; at least 1.
    pub queue: usize,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            queue: 8,
        }
    }
}

/// A queued job with its cancellation flag.
struct Work {
    spec: JobSpec,
    token: CancelToken,
}

/// Runs jobs on a fixed pool of workers.
///
/// Lives on the main thread. Every job passed to [`start`](Self::start) ends
/// with exactly one [`AppEvent::JobFinished`] through the sink, also when it
/// could not be queued, failed to start, was cancelled or panicked.
pub struct JobRunner {
    sender: Option<Sender<Work>>,
    workers: Vec<JoinHandle<()>>,
    tokens: HashMap<JobId, CancelToken>,
    sink: Arc<dyn EventSink>,
}

impl JobRunner {
    /// Starts the workers.
    pub fn new(
        executor: Arc<dyn OperationExecutor>,
        sink: Arc<dyn EventSink>,
        config: JobsConfig,
    ) -> io::Result<Self> {
        if config.workers == 0 || config.queue == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the job pool needs at least one worker and one queue slot",
            ));
        }
        let (sender, receiver) = crossbeam_channel::bounded(config.queue);
        let mut workers = Vec::with_capacity(config.workers);
        for index in 0..config.workers {
            let receiver = receiver.clone();
            let executor = Arc::clone(&executor);
            let sink = Arc::clone(&sink);
            workers.push(
                thread::Builder::new()
                    .name(format!("orbit-worker-{index}"))
                    .spawn(move || work(&receiver, executor.as_ref(), &sink))?,
            );
        }
        Ok(Self {
            sender: Some(sender),
            workers,
            tokens: HashMap::new(),
            sink,
        })
    }

    /// Queues a job. A full queue or stopped workers end it at once with
    /// [`JobError::QueueFull`] or [`JobError::Internal`].
    pub fn start(&mut self, spec: JobSpec) {
        let id = spec.id;
        let token = CancelToken::new();
        let work = Work {
            spec,
            token: token.clone(),
        };
        let sent = match &self.sender {
            Some(sender) => sender.try_send(work),
            None => Err(TrySendError::Disconnected(work)),
        };
        let error = match sent {
            Ok(()) => {
                self.tokens.insert(id, token);
                return;
            }
            Err(TrySendError::Full(_)) => {
                warn!(job = id.get(), "job queue is full");
                JobError::QueueFull
            }
            Err(TrySendError::Disconnected(_)) => {
                error!(job = id.get(), "job workers are stopped");
                JobError::Internal("workers stopped".into())
            }
        };
        self.sink.send(AppEvent::JobFinished {
            job: id,
            result: Err(error),
        });
    }

    /// Asks a queued or running job to stop.
    pub fn cancel(&self, job: JobId) {
        match self.tokens.get(&job) {
            Some(token) => token.cancel(),
            None => debug!(job = job.get(), "cancel of an unknown job ignored"),
        }
    }

    /// Forgets a job whose [`AppEvent::JobFinished`] arrived.
    pub fn finished(&mut self, job: JobId) {
        self.tokens.remove(&job);
    }

    /// Cancels every job, closes the queue and waits for the workers.
    /// Queued jobs end with [`JobError::Cancelled`].
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by tests until shutdown is wired")
    )]
    pub fn shutdown(&mut self) {
        for token in self.tokens.values() {
            token.cancel();
        }
        self.sender = None;
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                error!("job worker panicked");
            }
        }
    }

    /// Whether a cancel flag is kept for `job`.
    #[cfg(test)]
    pub(super) fn tracks(&self, job: JobId) -> bool {
        self.tokens.contains_key(&job)
    }
}

fn work(receiver: &Receiver<Work>, executor: &dyn OperationExecutor, sink: &Arc<dyn EventSink>) {
    while let Ok(Work { spec, token }) = receiver.recv() {
        let result = if token.is_cancelled() {
            Err(JobError::Cancelled)
        } else {
            run(&spec, token, executor, sink)
        };
        sink.send(AppEvent::JobFinished {
            job: spec.id,
            result,
        });
    }
    info!("job worker stopped");
}

fn run(
    spec: &JobSpec,
    token: CancelToken,
    executor: &dyn OperationExecutor,
    sink: &Arc<dyn EventSink>,
) -> Result<JobOutput, JobError> {
    let job = spec.id;
    let throttle = Mutex::new(Throttle::new(PROGRESS_INTERVAL));
    let progress_sink = Arc::clone(sink);
    let context = JobContext::new(token, move |progress| {
        let now = Instant::now();
        let admitted = match throttle.lock() {
            Ok(mut throttle) => throttle.admit(now),
            Err(poisoned) => poisoned.into_inner().admit(now),
        };
        if admitted {
            progress_sink.send(AppEvent::JobProgress { job, progress });
        }
    });
    panic::catch_unwind(AssertUnwindSafe(|| executor.execute(spec, &context))).unwrap_or_else(
        |_| {
            error!(job = job.get(), "operation panicked");
            Err(JobError::Internal("operation panicked".into()))
        },
    )
}

/// Lets an event through at most once per interval; the first one at once.
#[derive(Debug, Clone, Copy)]
struct Throttle {
    interval: Duration,
    last: Option<Instant>,
}

impl Throttle {
    const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }

    /// Whether an event at `now` goes through; if it does, the interval
    /// starts again from `now`.
    fn admit(&mut self, now: Instant) -> bool {
        match self.last {
            Some(last) if now.saturating_duration_since(last) < self.interval => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use orbit_app::messages::{Fraction, Progress};
    use orbit_domain::operation::OperationSpec;

    use super::*;
    use crate::runtime::testing::{ChannelSink, Gate, plain};

    /// An executor made of a closure.
    struct Fake<F>(F);

    impl<F> OperationExecutor for Fake<F>
    where
        F: Fn(&JobSpec, &JobContext) -> Result<JobOutput, JobError> + Send + Sync,
    {
        fn execute(&self, spec: &JobSpec, context: &JobContext) -> Result<JobOutput, JobError> {
            (self.0)(spec, context)
        }
    }

    fn output(spec: &JobSpec) -> Result<JobOutput, JobError> {
        Ok(JobOutput {
            path: spec.target.path.with_extension("zip"),
        })
    }

    fn spec(id: u64) -> JobSpec {
        let target = plain(&format!("/tmp/{id}"));
        let operation = OperationSpec::ArchiveZip;
        let destination = operation.destination(target.path.as_os_str());
        JobSpec {
            id: JobId::new(id),
            target,
            operation,
            destination,
        }
    }

    fn runner<F>(config: JobsConfig, execute: F) -> (JobRunner, ChannelSink)
    where
        F: Fn(&JobSpec, &JobContext) -> Result<JobOutput, JobError> + Send + Sync + 'static,
    {
        let sink = ChannelSink::new();
        match JobRunner::new(Arc::new(Fake(execute)), sink.shared(), config) {
            Ok(runner) => (runner, sink),
            Err(err) => panic!("runner: {err}"),
        }
    }

    fn finished(event: AppEvent) -> (u64, Result<JobOutput, JobError>) {
        match event {
            AppEvent::JobFinished { job, result } => (job.get(), result),
            other => panic!("not a finish: {other:?}"),
        }
    }

    const ONE: JobsConfig = JobsConfig {
        workers: 1,
        queue: 1,
    };

    #[test]
    fn finished_job_is_reported() {
        let (mut runner, sink) = runner(JobsConfig::default(), |spec, _| output(spec));
        runner.start(spec(1));
        assert!(runner.tracks(JobId::new(1)));
        assert_eq!(finished(sink.next()), (1, output(&spec(1))));
        runner.finished(JobId::new(1));
        assert!(!runner.tracks(JobId::new(1)));
        runner.shutdown();
    }

    #[test]
    fn full_queue_ends_the_job_at_once() {
        let gate = Gate::new();
        let held = gate.clone();
        let (mut runner, sink) = runner(ONE, move |spec, _| {
            if spec.id == JobId::new(1) {
                held.pass();
            }
            output(spec)
        });
        runner.start(spec(1));
        gate.wait_entered();
        runner.start(spec(2));
        runner.start(spec(3));
        assert_eq!(finished(sink.next()), (3, Err(JobError::QueueFull)));
        assert!(!runner.tracks(JobId::new(3)));
        gate.open();
        assert_eq!(finished(sink.next()).0, 1);
        assert_eq!(finished(sink.next()).0, 2);
        runner.shutdown();
    }

    #[test]
    fn job_cancelled_before_start_does_not_run() {
        let gate = Gate::new();
        let held = gate.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let (mut runner, sink) = runner(ONE, move |spec, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            if spec.id == JobId::new(1) {
                held.pass();
            }
            output(spec)
        });
        runner.start(spec(1));
        gate.wait_entered();
        runner.start(spec(2));
        runner.cancel(JobId::new(2));
        gate.open();
        assert_eq!(finished(sink.next()).0, 1);
        assert_eq!(finished(sink.next()), (2, Err(JobError::Cancelled)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        runner.shutdown();
    }

    #[test]
    fn running_job_sees_the_cancel() {
        let gate = Gate::new();
        let held = gate.clone();
        let (mut runner, sink) = runner(ONE, move |_, context| {
            held.pass();
            if context.is_cancelled() {
                Err(JobError::Cancelled)
            } else {
                Err(JobError::Internal("cancel not seen".into()))
            }
        });
        runner.start(spec(1));
        gate.wait_entered();
        runner.cancel(JobId::new(1));
        gate.open();
        assert_eq!(finished(sink.next()), (1, Err(JobError::Cancelled)));
        runner.shutdown();
    }

    #[test]
    fn panic_is_reported_and_the_worker_goes_on() {
        let (mut runner, sink) = runner(ONE, |spec, _| {
            if spec.id == JobId::new(1) {
                panic!("fake operation panics");
            }
            output(spec)
        });
        runner.start(spec(1));
        assert_eq!(
            finished(sink.next()),
            (1, Err(JobError::Internal("operation panicked".into())))
        );
        runner.start(spec(2));
        assert_eq!(finished(sink.next()), (2, output(&spec(2))));
        runner.shutdown();
    }

    #[test]
    fn throttle_admits_once_per_interval() {
        let base = Instant::now();
        let mut throttle = Throttle::new(Duration::from_millis(100));
        let admitted: Vec<u64> = [0, 99, 100, 199, 200]
            .into_iter()
            .filter(|&ms| throttle.admit(base + Duration::from_millis(ms)))
            .collect();
        assert_eq!(admitted, vec![0, 100, 200]);
    }

    #[test]
    fn progress_from_a_helper_thread_comes_before_the_finish() {
        let fractions = [0.25, 0.5, 1.0];
        let (mut runner, sink) = runner(ONE, move |spec, context| {
            thread::scope(|scope| {
                scope.spawn(|| {
                    for (index, value) in fractions.into_iter().enumerate() {
                        if index > 0 {
                            thread::sleep(PROGRESS_INTERVAL + Duration::from_millis(20));
                        }
                        if let Some(fraction) = Fraction::new(value) {
                            context.report(Progress::Fraction(fraction));
                        }
                    }
                });
            });
            output(spec)
        });
        runner.start(spec(1));
        let mut reported = Vec::new();
        loop {
            match sink.next() {
                AppEvent::JobProgress {
                    progress: Progress::Fraction(fraction),
                    ..
                } => reported.push(fraction.get()),
                event => {
                    assert_eq!(finished(event), (1, output(&spec(1))));
                    break;
                }
            }
        }
        assert_eq!(reported, fractions);
        runner.shutdown();
    }

    #[test]
    fn shutdown_cancels_queued_jobs_and_later_starts_fail() {
        let gate = Gate::new();
        let held = gate.clone();
        let (mut runner, sink) = runner(ONE, move |spec, context| {
            held.pass();
            if context.is_cancelled() {
                return Err(JobError::Cancelled);
            }
            output(spec)
        });
        runner.start(spec(1));
        gate.wait_entered();
        runner.start(spec(2));
        let opener = gate.clone();
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            opener.open();
        });
        runner.shutdown();
        if release.join().is_err() {
            panic!("gate thread panicked");
        }
        assert_eq!(finished(sink.next()), (1, Err(JobError::Cancelled)));
        assert_eq!(finished(sink.next()), (2, Err(JobError::Cancelled)));
        runner.start(spec(3));
        assert_eq!(
            finished(sink.next()),
            (3, Err(JobError::Internal("workers stopped".into())))
        );
        sink.assert_empty();
    }
}
