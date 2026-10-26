//! Target resolution thread.

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender};
use orbit_app::messages::{AppEvent, ResolveError};
use orbit_app::ports::{EventSink, ResolveRequest, TargetResolver};
use tracing::{debug, error, info};

/// The resolver thread is not running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolverStopped;

/// Runs a [`TargetResolver`] on its own thread and sends its answers as
/// [`AppEvent::TargetResolved`].
///
/// Only the newest waiting request is resolved; older ones are dropped
/// without an answer. The controller asks for a target only when a new
/// gesture starts, so a dropped request never belongs to the current one.
#[derive(Debug)]
pub struct ResolverThread {
    sender: Option<Sender<ResolveRequest>>,
    thread: Option<JoinHandle<()>>,
}

impl ResolverThread {
    /// Starts the thread. The resolver is made by `make` on that thread and
    /// dropped there, so it does not have to be `Send`.
    pub fn spawn<F, R>(make: F, sink: Arc<dyn EventSink>) -> io::Result<Self>
    where
        F: FnOnce() -> R + Send + 'static,
        R: TargetResolver + 'static,
    {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let thread = thread::Builder::new()
            .name("orbit-resolver".into())
            .spawn(move || serve(make(), &receiver, sink.as_ref()))?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    /// Queues a request.
    pub fn request(&self, request: ResolveRequest) -> Result<(), ResolverStopped> {
        self.sender
            .as_ref()
            .ok_or(ResolverStopped)?
            .send(request)
            .map_err(|_| ResolverStopped)
    }

    /// Stops the thread and waits for it. Does nothing the second time.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by tests until shutdown is wired")
    )]
    pub fn shutdown(&mut self) {
        self.sender = None;
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            error!("resolver thread panicked");
        }
    }
}

fn serve<R: TargetResolver>(
    mut resolver: R,
    receiver: &Receiver<ResolveRequest>,
    sink: &dyn EventSink,
) {
    while let Ok(mut request) = receiver.recv() {
        while let Ok(newer) = receiver.try_recv() {
            debug!(gesture = ?request.gesture, "resolve request superseded");
            request = newer;
        }
        let result = panic::catch_unwind(AssertUnwindSafe(|| resolver.resolve(&request)))
            .unwrap_or_else(|_| {
                error!(gesture = ?request.gesture, "resolver panicked");
                Err(ResolveError::Failed("resolver panicked".into()))
            });
        sink.send(AppEvent::TargetResolved {
            gesture: request.gesture,
            result,
        });
    }
    info!("resolver thread stopped");
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::Instant;

    use orbit_app::messages::GestureId;
    use orbit_domain::target::FileTarget;

    use super::*;
    use crate::runtime::testing::{ChannelSink, Gate, plain};

    /// Answers with a target named after the gesture. Holds an `Rc`, so it
    /// is not `Send`; it can run only because it is made on its thread.
    struct Fake {
        _not_send: Rc<()>,
        gate: Option<Gate>,
        panic_on: Option<GestureId>,
    }

    impl TargetResolver for Fake {
        fn resolve(&mut self, request: &ResolveRequest) -> Result<FileTarget, ResolveError> {
            if let Some(gate) = self.gate.take() {
                gate.pass();
            }
            if self.panic_on == Some(request.gesture) {
                panic!("fake resolver panics");
            }
            Ok(plain(&format!("/tmp/{}", request.gesture.0)))
        }
    }

    fn request(id: u64) -> ResolveRequest {
        ResolveRequest {
            gesture: GestureId(id),
            pressed_at: Instant::now(),
        }
    }

    fn spawn(gate: Option<Gate>, panic_on: Option<GestureId>) -> (ResolverThread, ChannelSink) {
        let sink = ChannelSink::new();
        let thread = ResolverThread::spawn(
            move || Fake {
                _not_send: Rc::new(()),
                gate,
                panic_on,
            },
            sink.shared(),
        );
        match thread {
            Ok(thread) => (thread, sink),
            Err(err) => panic!("spawn: {err}"),
        }
    }

    fn answered(event: AppEvent) -> (u64, Result<FileTarget, ResolveError>) {
        match event {
            AppEvent::TargetResolved { gesture, result } => (gesture.0, result),
            other => panic!("not an answer: {other:?}"),
        }
    }

    #[test]
    fn answer_carries_the_gesture() {
        let (mut thread, sink) = spawn(None, None);
        assert_eq!(thread.request(request(7)), Ok(()));
        assert_eq!(answered(sink.next()), (7, Ok(plain("/tmp/7"))));
        thread.shutdown();
    }

    #[test]
    fn newest_waiting_request_wins() {
        let gate = Gate::new();
        let (mut thread, sink) = spawn(Some(gate.clone()), None);
        assert_eq!(thread.request(request(1)), Ok(()));
        gate.wait_entered();
        assert_eq!(thread.request(request(2)), Ok(()));
        assert_eq!(thread.request(request(3)), Ok(()));
        gate.open();
        assert_eq!(answered(sink.next()).0, 1);
        assert_eq!(answered(sink.next()).0, 3);
        thread.shutdown();
        sink.assert_empty();
    }

    #[test]
    fn panic_is_answered_and_the_thread_goes_on() {
        let (mut thread, sink) = spawn(None, Some(GestureId(1)));
        assert_eq!(thread.request(request(1)), Ok(()));
        assert_eq!(
            answered(sink.next()),
            (1, Err(ResolveError::Failed("resolver panicked".into())))
        );
        assert_eq!(thread.request(request(2)), Ok(()));
        assert_eq!(answered(sink.next()).0, 2);
        thread.shutdown();
    }

    #[test]
    fn request_after_shutdown_fails() {
        let (mut thread, _sink) = spawn(None, None);
        thread.shutdown();
        assert_eq!(thread.request(request(1)), Err(ResolverStopped));
        thread.shutdown();
        assert_eq!(thread.request(request(2)), Err(ResolverStopped));
    }
}
