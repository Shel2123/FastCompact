//! Application runtime: the controller with the threads and ports that
//! carry out its effects.

use std::io;
use std::sync::Arc;

use orbit_app::controller::AppController;
use orbit_app::messages::AppEvent;
use orbit_app::ports::{EventSink, OperationExecutor};
use orbit_domain::operation::FileNamePolicy;
use thiserror::Error;

use crate::effects::{self, Services};
use crate::jobs::{JobRunner, JobsConfig};
use crate::platform::{Platform, PlatformError, ResolverFactory, Views};
use crate::resolver::ResolverThread;

/// The controller and the services its effects use. Lives on the main
/// thread and takes events from the [`EventSink`] one at a time.
pub struct Runtime {
    controller: AppController,
    services: Services,
}

impl Runtime {
    /// Starts the resolver thread and the job workers. The resolver is made
    /// on its thread.
    ///
    /// # Errors
    ///
    /// [`io::Error`] when a thread cannot be started or `jobs` is empty.
    pub fn new(
        sink: Arc<dyn EventSink>,
        resolver: ResolverFactory,
        executor: Arc<dyn OperationExecutor>,
        names: FileNamePolicy,
        views: Views,
        jobs: JobsConfig,
    ) -> io::Result<Self> {
        let resolver = ResolverThread::spawn(resolver, Arc::clone(&sink))?;
        let jobs = JobRunner::new(executor, Arc::clone(&sink), jobs)?;
        Ok(Self {
            controller: AppController::new(names),
            services: Services {
                resolver,
                jobs,
                sink,
                views,
            },
        })
    }

    /// Handles one event and carries out the effects in order.
    pub fn handle(&mut self, event: AppEvent) {
        if let AppEvent::JobFinished { job, .. } = &event {
            self.services.jobs.finished(*job);
        }
        for effect in self.controller.update(event) {
            effects::execute(effect, &mut self.services);
        }
    }
}

/// Tuning of the runtime.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeConfig {
    /// Workers and queue of the job runner.
    pub jobs: JobsConfig,
}

/// Why [`run`] stopped with an error.
#[derive(Debug, Error)]
pub enum RunError {
    /// A thread of the runtime could not be started.
    #[error("the runtime could not start: {0}")]
    Start(#[from] io::Error),
    /// The platform failed.
    #[error("the platform failed: {0}")]
    Platform(PlatformError),
}

/// Runs the application on `platform` until it quits.
///
/// Builds the runtime from the parts of the platform, then hands the
/// platform the handler of the controller and lets it run the event loop.
///
/// # Errors
///
/// [`RunError::Start`] when the runtime cannot start, [`RunError::Platform`]
/// when the platform fails.
pub fn run<P: Platform>(mut platform: P, config: RuntimeConfig) -> Result<(), RunError> {
    let mut runtime = Runtime::new(
        platform.sink(),
        platform.resolver(),
        platform.executor(),
        platform.file_name_policy(),
        platform.views(),
        config.jobs,
    )?;
    platform
        .run(Box::new(move |event| runtime.handle(event)))
        .map_err(RunError::Platform)
}

#[cfg(test)]
mod tests {

    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Mutex;

    use orbit_app::messages::{GestureId, JobError, JobOutput, JobSpec, Notice, ResolveError};
    use orbit_app::ports::{JobContext, ResolveRequest, TargetResolver};
    use orbit_app::state::MenuState;
    use orbit_domain::action::{ActionId, MENU_LAYOUT, MenuItem};
    use orbit_domain::geometry::{MenuGeometry, ScreenPoint, ScreenRect};
    use orbit_domain::target::FileTarget;

    use crate::platform::Handler;
    use crate::testing::{Call, ChannelSink, Gate, Journal, plain, views};

    use super::*;

    const A: GestureId = GestureId(1);

    struct Resolves;

    impl TargetResolver for Resolves {
        fn resolve(&mut self, _: &ResolveRequest) -> Result<FileTarget, ResolveError> {
            Ok(plain("/tmp/Docs"))
        }
    }

    /// Zips instantly, or waits at the gate first.
    struct Zips(Option<Gate>);

    impl OperationExecutor for Zips {
        fn execute(&self, spec: &JobSpec, _: &JobContext) -> Result<JobOutput, JobError> {
            if let Some(gate) = &self.0 {
                gate.pass();
            }
            Ok(JobOutput {
                path: spec.target.path.with_extension("zip"),
            })
        }
    }

    fn runtime(gate: Option<Gate>, jobs: JobsConfig) -> (Runtime, ChannelSink) {
        let sink = ChannelSink::new();
        let made = Runtime::new(
            sink.shared(),
            ResolverFactory::new(|| Resolves),
            Arc::new(Zips(gate)),
            FileNamePolicy::UNIVERSAL,
            views(&Journal::new(), None),
            jobs,
        );
        match made {
            Ok(runtime) => (runtime, sink),
            Err(err) => panic!("runtime: {err}"),
        }
    }

    fn invoked(gesture: GestureId) -> AppEvent {
        AppEvent::Invoked {
            gesture,
            point: ScreenPoint::new(700.0, 400.0),
            screens: vec![ScreenRect::new(0.0, 0.0, 1440.0, 875.0)],
        }
    }

    /// Opens the menu of `gesture` with the answer of the resolver thread
    /// and returns its center.
    fn open_menu(runtime: &mut Runtime, sink: &ChannelSink, gesture: GestureId) -> ScreenPoint {
        runtime.handle(invoked(gesture));
        runtime.handle(sink.next());
        match runtime.controller.state().menu {
            MenuState::Open { center, .. } => center,
            ref other => panic!("menu not open: {other:?}"),
        }
    }

    /// Runs a whole gesture that ends over the ZIP slot.
    fn zip(runtime: &mut Runtime, sink: &ChannelSink, gesture: GestureId) {
        let center = open_menu(runtime, sink, gesture);
        let Some(slot) = MENU_LAYOUT
            .iter()
            .position(|item| *item == MenuItem::Action(ActionId::ArchiveZip))
        else {
            panic!("ZIP is not in the menu");
        };
        let angle = MenuGeometry::standard().slot_center(slot);
        let point = ScreenPoint::new(center.x + 70.0 * angle.sin(), center.y + 70.0 * angle.cos());
        runtime.handle(AppEvent::PointerMoved { gesture, point });
        runtime.handle(AppEvent::Released { gesture, point });
    }

    #[test]
    fn target_from_the_resolver_thread_opens_the_menu() {
        let (mut runtime, sink) = runtime(None, JobsConfig::default());
        open_menu(&mut runtime, &sink, A);
        sink.assert_empty();
    }

    #[test]
    fn job_runs_and_its_finish_clears_the_state() {
        let (mut runtime, sink) = runtime(None, JobsConfig::default());
        zip(&mut runtime, &sink, A);
        assert_eq!(runtime.controller.state().jobs.len(), 1);
        let finish = sink.next();
        let AppEvent::JobFinished { job, result } = &finish else {
            panic!("not a finish: {finish:?}");
        };
        let job = *job;
        assert_eq!(
            result.as_ref().map(|output| output.path.clone()),
            Ok("/tmp/Docs.zip".into())
        );
        assert!(runtime.services.jobs.tracks(job));
        runtime.handle(finish);
        assert!(runtime.controller.state().jobs.is_empty());
        assert!(!runtime.services.jobs.tracks(job));
    }

    #[test]
    fn stopped_resolver_refuses_the_gesture() {
        let (mut runtime, sink) = runtime(None, JobsConfig::default());
        runtime.services.resolver.shutdown();
        runtime.handle(invoked(A));
        let answer = sink.next();
        assert_eq!(
            answer,
            AppEvent::TargetResolved {
                gesture: A,
                result: Err(ResolveError::Failed("resolver stopped".into())),
            }
        );
        runtime.handle(answer);
        assert_eq!(runtime.controller.state().menu, MenuState::Closed);
    }

    #[test]
    fn job_that_does_not_fit_is_removed() {
        let gate = Gate::new();
        let (mut runtime, sink) = runtime(
            Some(gate.clone()),
            JobsConfig {
                workers: 1,
                queue: 1,
            },
        );
        // The first job holds the worker, the second fills the queue.
        zip(&mut runtime, &sink, GestureId(1));
        gate.wait_entered();
        zip(&mut runtime, &sink, GestureId(2));
        zip(&mut runtime, &sink, GestureId(3));
        assert_eq!(runtime.controller.state().jobs.len(), 3);
        let full = sink.next();
        assert!(matches!(
            full,
            AppEvent::JobFinished {
                result: Err(JobError::QueueFull),
                ..
            }
        ));
        runtime.handle(full);
        assert_eq!(runtime.controller.state().jobs.len(), 2);
        gate.open();
        gate.wait_entered();
        gate.open();
        for _ in 0..2 {
            let finish = sink.next();
            runtime.handle(finish);
        }
        assert!(runtime.controller.state().jobs.is_empty());
    }

    /// The order in which `run` used the parts of a [`FakePlatform`].
    type Steps = Rc<RefCell<Vec<&'static str>>>;

    /// A platform without an event loop: `run` hands the events that were
    /// sent to its sink to the handler, in order, and returns.
    struct FakePlatform {
        steps: Steps,
        journal: Journal,
        queue: Arc<Mutex<Vec<AppEvent>>>,
        run_error: Option<&'static str>,
    }

    struct QueueSink(Arc<Mutex<Vec<AppEvent>>>);

    impl EventSink for QueueSink {
        fn send(&self, event: AppEvent) {
            if let Ok(mut queue) = self.0.lock() {
                queue.push(event);
            }
        }
    }

    impl FakePlatform {
        fn new() -> Self {
            Self {
                steps: Steps::default(),
                journal: Journal::new(),
                queue: Arc::default(),
                run_error: None,
            }
        }
    }

    impl Platform for FakePlatform {
        fn sink(&self) -> Arc<dyn EventSink> {
            self.steps.borrow_mut().push("sink");
            Arc::new(QueueSink(Arc::clone(&self.queue)))
        }

        fn resolver(&self) -> ResolverFactory {
            self.steps.borrow_mut().push("resolver");
            ResolverFactory::new(|| Resolves)
        }

        fn executor(&self) -> Arc<dyn OperationExecutor> {
            self.steps.borrow_mut().push("executor");
            Arc::new(Zips(None))
        }

        fn file_name_policy(&self) -> FileNamePolicy {
            self.steps.borrow_mut().push("policy");
            FileNamePolicy::UNIVERSAL
        }

        fn views(&mut self) -> Views {
            self.steps.borrow_mut().push("views");
            views(&self.journal, None)
        }

        fn run(self, mut handler: Handler) -> Result<(), PlatformError> {
            self.steps.borrow_mut().push("run");
            let events = self
                .queue
                .lock()
                .map(|mut queue| std::mem::take(&mut *queue));
            for event in events.unwrap_or_default() {
                handler(event);
            }
            self.run_error.map_or(Ok(()), |message| Err(message.into()))
        }
    }

    fn refused(reason: ResolveError) -> AppEvent {
        AppEvent::InvokeRefused { reason }
    }

    #[test]
    fn run_asks_for_the_parts_and_then_hands_over_the_loop() {
        let platform = FakePlatform::new();
        let steps = Rc::clone(&platform.steps);
        let journal = platform.journal.clone();
        // Sent through the sink before `run`: kept and delivered in order.
        let sink = platform.sink();
        sink.send(refused(ResolveError::SecureInputOn));
        sink.send(refused(ResolveError::AccessibilityDenied));
        steps.borrow_mut().clear();

        let result = run(platform, RuntimeConfig::default());

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            *steps.borrow(),
            ["sink", "resolver", "executor", "policy", "views", "run"]
        );
        assert_eq!(
            journal.take(),
            vec![
                Call::Notify(Notice::Refused(ResolveError::SecureInputOn)),
                Call::Notify(Notice::Refused(ResolveError::AccessibilityDenied)),
            ]
        );
    }

    #[test]
    fn platform_failure_is_returned() {
        let mut platform = FakePlatform::new();
        platform.run_error = Some("no event loop");
        let result = run(platform, RuntimeConfig::default());
        assert!(
            matches!(&result, Err(RunError::Platform(err)) if err.to_string() == "no event loop"),
            "{result:?}"
        );
    }

    #[test]
    fn runtime_that_cannot_start_never_reaches_the_loop() {
        let platform = FakePlatform::new();
        let steps = Rc::clone(&platform.steps);
        let config = RuntimeConfig {
            jobs: JobsConfig {
                workers: 0,
                queue: 0,
            },
        };
        let result = run(platform, config);
        assert!(matches!(result, Err(RunError::Start(_))), "{result:?}");
        assert!(!steps.borrow().contains(&"run"));
    }
}
