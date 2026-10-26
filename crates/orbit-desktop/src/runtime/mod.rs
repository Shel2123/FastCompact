//! Application runtime: the controller with the threads that carry out its
//! effects.

mod effects;
mod jobs;
mod resolver;

use std::io;
use std::sync::Arc;

use orbit_app::controller::AppController;
use orbit_app::messages::AppEvent;
use orbit_app::ports::{EventSink, OperationExecutor, TargetResolver};

use effects::Services;
use jobs::JobRunner;
pub use jobs::JobsConfig;
use resolver::ResolverThread;

/// The controller and the services its effects use. Lives on the main
/// thread and takes events from the [`EventSink`] one at a time.
pub struct Runtime {
    controller: AppController,
    services: Services,
}

impl Runtime {
    /// Starts the resolver thread and the job workers. `make_resolver` runs
    /// on the resolver thread.
    pub fn new<F, R>(
        sink: Arc<dyn EventSink>,
        make_resolver: F,
        executor: Arc<dyn OperationExecutor>,
        jobs: JobsConfig,
    ) -> io::Result<Self>
    where
        F: FnOnce() -> R + Send + 'static,
        R: TargetResolver + 'static,
    {
        let resolver = ResolverThread::spawn(make_resolver, Arc::clone(&sink))?;
        let jobs = JobRunner::new(executor, Arc::clone(&sink), jobs)?;
        Ok(Self {
            controller: AppController::new(),
            services: Services {
                resolver,
                jobs,
                sink,
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

#[cfg(test)]
pub(crate) mod testing {
    //! Fakes shared by the runtime tests.

    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use crossbeam_channel::{Receiver, Sender};
    use orbit_app::messages::AppEvent;
    use orbit_app::ports::EventSink;
    use orbit_domain::target::{FileFacts, FileIdentity, FileKind, FileTarget, FileVersion};

    /// Longest wait for an event before a test fails.
    const TIMEOUT: Duration = Duration::from_secs(5);

    /// An [`EventSink`] that the test reads in order.
    pub struct ChannelSink {
        sender: Sender<AppEvent>,
        receiver: Receiver<AppEvent>,
    }

    impl ChannelSink {
        pub fn new() -> Self {
            let (sender, receiver) = crossbeam_channel::unbounded();
            Self { sender, receiver }
        }

        pub fn shared(&self) -> Arc<dyn EventSink> {
            Arc::new(SenderSink(self.sender.clone()))
        }

        /// The next event; fails the test after [`TIMEOUT`].
        pub fn next(&self) -> AppEvent {
            match self.receiver.recv_timeout(TIMEOUT) {
                Ok(event) => event,
                Err(err) => panic!("no event: {err}"),
            }
        }

        pub fn assert_empty(&self) {
            if let Ok(event) = self.receiver.try_recv() {
                panic!("unexpected event: {event:?}");
            }
        }
    }

    struct SenderSink(Sender<AppEvent>);

    impl EventSink for SenderSink {
        fn send(&self, event: AppEvent) {
            // The test may have stopped reading; nothing to do then.
            let _ = self.0.send(event);
        }
    }

    /// Holds a fake inside its call until the test opens it.
    #[derive(Clone)]
    pub struct Gate {
        entered: (Sender<()>, Receiver<()>),
        open: (Sender<()>, Receiver<()>),
    }

    impl Gate {
        pub fn new() -> Self {
            Self {
                entered: crossbeam_channel::unbounded(),
                open: crossbeam_channel::unbounded(),
            }
        }

        /// Called by the fake: tells the test it entered, then waits.
        pub fn pass(&self) {
            let _ = self.entered.0.send(());
            if self.open.1.recv_timeout(TIMEOUT).is_err() {
                panic!("gate was not opened");
            }
        }

        /// Waits until the fake is inside its call.
        pub fn wait_entered(&self) {
            if self.entered.1.recv_timeout(TIMEOUT).is_err() {
                panic!("fake did not enter the gate");
            }
        }

        /// Lets one waiting call go on.
        pub fn open(&self) {
            let _ = self.open.0.send(());
        }
    }

    /// A plain file target at `path`.
    pub fn plain(path: &str) -> FileTarget {
        FileTarget {
            path: PathBuf::from(path),
            identity: FileIdentity {
                device: 1,
                inode: 42,
            },
            version: FileVersion {
                size: 1024,
                modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            },
            facts: FileFacts {
                kind: FileKind::File,
                image: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use orbit_app::messages::{GestureId, JobError, JobOutput, JobSpec, ResolveError};
    use orbit_app::ports::{JobContext, ResolveRequest};
    use orbit_app::state::MenuState;
    use orbit_domain::action::{ActionId, MENU_LAYOUT, MenuItem};
    use orbit_domain::geometry::{MenuGeometry, ScreenPoint, ScreenRect};
    use orbit_domain::target::FileTarget;

    use super::testing::{ChannelSink, Gate, plain};
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
        match Runtime::new(sink.shared(), || Resolves, Arc::new(Zips(gate)), jobs) {
            Ok(runtime) => (runtime, sink),
            Err(err) => panic!("runtime: {err}"),
        }
    }

    fn invoked(gesture: GestureId) -> AppEvent {
        AppEvent::Invoked {
            gesture,
            point: ScreenPoint::new(700.0, 400.0),
            screens: vec![ScreenRect::new(0.0, 0.0, 1440.0, 875.0)],
            pressed_at: Instant::now(),
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
}
