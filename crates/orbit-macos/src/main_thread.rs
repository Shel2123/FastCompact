//! Event delivery to the AppKit main thread.
//!
//! [`MainThreadSender`] puts each event on the main dispatch queue. On the
//! main thread a mailbox hands the events to the handler one at a time, so
//! the handler is never entered again while it runs, even when it spins a
//! nested run loop such as a modal window.

use std::cell::{OnceCell, RefCell};
use std::collections::VecDeque;

use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use orbit_app::messages::AppEvent;
use orbit_app::ports::EventSink;
use thiserror::Error;
use tracing::warn;

thread_local! {
    static MAILBOX: OnceCell<Mailbox<AppEvent>> = const { OnceCell::new() };
}

/// Why the main-thread delivery could not be set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MainThreadError {
    /// [`MainThreadSender::new`] was called before.
    #[error("the main-thread sender already exists")]
    AlreadyCreated,
    /// [`MainThreadSender::new`] was not called.
    #[error("the main-thread sender does not exist")]
    NotCreated,
    /// The handler was set before.
    #[error("the main-thread handler is already set")]
    HandlerAlreadySet,
}

/// Sends events to the handler on the main thread.
///
/// Implements [`EventSink`]: the main dispatch queue is serial, and
/// `dispatch_async` has queued the event when it returns, so events keep the
/// order of their `send` calls across threads. The handler runs later from
/// the main run loop, never inside `send`.
#[derive(Debug, Clone, Copy)]
pub struct MainThreadSender {
    _private: (),
}

impl MainThreadSender {
    /// Creates the mailbox on the main thread. Events sent before
    /// [`set_handler`](Self::set_handler) wait for the handler.
    pub fn new(_mtm: MainThreadMarker) -> Result<Self, MainThreadError> {
        MAILBOX.with(|cell| {
            cell.set(Mailbox::new())
                .map_err(|_| MainThreadError::AlreadyCreated)
        })?;
        Ok(Self { _private: () })
    }

    /// Sets the handler and hands it the events that are waiting.
    pub fn set_handler(
        &self,
        _mtm: MainThreadMarker,
        handler: impl FnMut(AppEvent) + 'static,
    ) -> Result<(), MainThreadError> {
        MAILBOX.with(|cell| {
            cell.get()
                .ok_or(MainThreadError::NotCreated)?
                .set_handler(handler)
        })
    }
}

impl EventSink for MainThreadSender {
    fn send(&self, event: AppEvent) {
        DispatchQueue::main().exec_async(move || {
            MAILBOX.with(|cell| match cell.get() {
                Some(mailbox) => mailbox.deliver(event),
                None => warn!(?event, "no main-thread mailbox; event dropped"),
            });
        });
    }
}

/// The handler of a [`Mailbox`].
type Handler<E> = Box<dyn FnMut(E)>;

/// Queue of events for one handler on one thread.
///
/// The queue and the handler sit in separate cells: an event delivered while
/// the handler runs is only queued, and the running drain loop takes it after
/// the current event.
struct Mailbox<E> {
    queue: RefCell<VecDeque<E>>,
    handler: RefCell<Option<Handler<E>>>,
}

impl<E> Mailbox<E> {
    fn new() -> Self {
        Self {
            queue: RefCell::new(VecDeque::new()),
            handler: RefCell::new(None),
        }
    }

    /// Queues `event` and, unless the handler is running or not set yet,
    /// hands it every queued event.
    fn deliver(&self, event: E) {
        self.queue.borrow_mut().push_back(event);
        self.drain();
    }

    fn set_handler(&self, handler: impl FnMut(E) + 'static) -> Result<(), MainThreadError> {
        {
            let mut slot = self
                .handler
                .try_borrow_mut()
                .map_err(|_| MainThreadError::HandlerAlreadySet)?;
            if slot.is_some() {
                return Err(MainThreadError::HandlerAlreadySet);
            }
            *slot = Some(Box::new(handler));
        }
        self.drain();
        Ok(())
    }

    fn drain(&self) {
        // Busy means this call is nested inside the handler: the outer loop
        // takes the event once the handler returns.
        let Ok(mut handler) = self.handler.try_borrow_mut() else {
            return;
        };
        let Some(handler) = handler.as_mut() else {
            return;
        };
        loop {
            let next = self.queue.borrow_mut().pop_front();
            let Some(event) = next else {
                break;
            };
            handler(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    fn recording(mailbox: &Mailbox<u32>) -> Rc<RefCell<Vec<u32>>> {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let log = Rc::clone(&seen);
        assert_eq!(
            mailbox.set_handler(move |event| log.borrow_mut().push(event)),
            Ok(())
        );
        seen
    }

    #[test]
    fn keeps_order() {
        let mailbox = Mailbox::new();
        let seen = recording(&mailbox);
        for event in 1..=5 {
            mailbox.deliver(event);
        }
        assert_eq!(*seen.borrow(), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn nested_delivery_waits_for_the_running_handler() {
        let mailbox = Rc::new(Mailbox::new());
        let seen = Rc::new(RefCell::new(Vec::new()));
        let depth = Rc::new(RefCell::new(0_u32));
        let (inner, log, level) = (Rc::downgrade(&mailbox), Rc::clone(&seen), Rc::clone(&depth));
        let handler = move |event: u32| {
            *level.borrow_mut() += 1;
            log.borrow_mut().push((event, *level.borrow()));
            if event == 1 {
                // A nested run loop inside the handler delivers more events.
                if let Some(mailbox) = inner.upgrade() {
                    mailbox.deliver(2);
                    mailbox.deliver(3);
                }
            }
            *level.borrow_mut() -= 1;
        };
        assert_eq!(mailbox.set_handler(handler), Ok(()));
        mailbox.deliver(1);
        assert_eq!(*seen.borrow(), vec![(1, 1), (2, 1), (3, 1)]);
    }

    #[test]
    fn buffers_events_until_the_handler_is_set() {
        let mailbox = Mailbox::new();
        mailbox.deliver(1);
        mailbox.deliver(2);
        let seen = recording(&mailbox);
        assert_eq!(*seen.borrow(), vec![1, 2]);
        mailbox.deliver(3);
        assert_eq!(*seen.borrow(), vec![1, 2, 3]);
    }

    #[test]
    fn handler_is_set_once() {
        let mailbox = Mailbox::<u32>::new();
        let _seen = recording(&mailbox);
        assert_eq!(
            mailbox.set_handler(|_| {}),
            Err(MainThreadError::HandlerAlreadySet)
        );
    }
}
