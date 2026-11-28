//! The jobs the user sees: the indicator in the menu bar, the timer that
//! shows a job once it has lasted, and Quit while jobs run.
//!
//! Each timer is one `NSTimer` in the common modes of the main run loop. So
//! it fires while a menu of the status item is open, which AppKit tracks in
//! `NSEventTrackingRunLoopMode`, and while Quit waits, which AppKit does in
//! `NSModalPanelRunLoopMode`. A quiet job, such as one of ditto, is shown
//! by the timer without any event.

use std::cell::RefCell;
use std::fmt;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::MainThreadBound;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSTimer};
use orbit_app::messages::{AppEvent, JobId, JobSpec, Progress};
use orbit_app::ports::EventSink;
use tracing::{info, warn};

use super::jobs_indicator::{Icon, JobRow, JobsIndicator};

/// How long Quit waits for cancelled jobs to end.
pub const QUIT_GRACE: Duration = Duration::from_secs(2);

/// Where the jobs are shown.
pub trait JobsView {
    /// Shows `rows`, replacing what is shown, and `icon` in the menu bar.
    fn show(&mut self, rows: &[JobRow], icon: Icon);
}

/// How a Quit that waited for jobs ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitEnd {
    /// The last job reported its result.
    Finished,
    /// The grace period ran out with jobs still running.
    Deadline,
}

/// What Quit must do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitReply {
    /// No job runs: quit at once.
    Now,
    /// The jobs are cancelled; the callback of [`Jobs::quit`] says when to
    /// quit.
    Later,
}

/// The jobs between `job_started` and their result, and their view; cheap
/// to clone, main thread only.
#[derive(Clone)]
pub struct Jobs {
    inner: Rc<RefCell<Inner>>,
}

struct Inner {
    mtm: MainThreadMarker,
    this: Weak<RefCell<Inner>>,
    indicator: JobsIndicator,
    view: Box<dyn JobsView>,
    sink: Arc<dyn EventSink>,
    /// The timer that shows the next hidden job, and when it fires.
    timer: Option<(Instant, Retained<NSTimer>)>,
    quit: Option<Quit>,
}

/// A Quit that waits for the cancelled jobs.
struct Quit {
    deadline: Retained<NSTimer>,
    end: Box<dyn FnOnce(QuitEnd)>,
}

impl Jobs {
    /// No jobs yet. `sink` takes the cancellations of Quit.
    pub fn new(mtm: MainThreadMarker, view: Box<dyn JobsView>, sink: Arc<dyn EventSink>) -> Self {
        let inner = Rc::new_cyclic(|this| {
            RefCell::new(Inner {
                mtm,
                this: this.clone(),
                indicator: JobsIndicator::new(),
                view,
                sink,
                timer: None,
                quit: None,
            })
        });
        Self { inner }
    }

    /// A job was queued. While Quit waits, it is cancelled at once.
    pub fn started(&self, spec: &JobSpec) {
        self.with(|inner| {
            inner.indicator.started(spec, Instant::now());
            if inner.quit.is_some() {
                inner.cancel(spec.id);
            }
            inner.update();
        });
    }

    /// A worker reported `progress` for `job`.
    pub fn progress(&self, job: JobId, progress: Progress) {
        self.with(|inner| {
            inner.indicator.progress(job, progress);
            inner.update();
        });
    }

    /// `job` has its result. The last one ends a waiting Quit.
    pub fn finished(&self, job: JobId) {
        let end = self.with(|inner| {
            inner.indicator.finished(job);
            inner.update();
            if inner.indicator.is_empty() {
                inner.end_quit()
            } else {
                None
            }
        });
        // Outside the borrow: the callback may quit the application.
        if let Some(end) = end.flatten() {
            end(QuitEnd::Finished);
        }
    }

    /// Whether Quit waits for jobs.
    pub fn is_quitting(&self) -> bool {
        self.inner
            .try_borrow()
            .map(|inner| inner.quit.is_some())
            .unwrap_or(false)
    }

    /// The user asked to quit. With no job, says to quit now. Otherwise
    /// cancels every job, queued and hidden ones too, and calls `end` once:
    /// with [`QuitEnd::Finished`] when the last job reports its result, or
    /// with [`QuitEnd::Deadline`] after `grace`. A second request while one
    /// waits keeps the first callback.
    pub fn quit(&self, grace: Duration, end: impl FnOnce(QuitEnd) + 'static) -> QuitReply {
        self.with(|inner| {
            if inner.quit.is_some() {
                return QuitReply::Later;
            }
            let jobs = inner.indicator.jobs();
            if jobs.is_empty() {
                return QuitReply::Now;
            }
            info!(jobs = jobs.len(), ?grace, "quit waits for the jobs");
            for job in jobs {
                inner.cancel(job);
            }
            inner.quit = Some(Quit {
                deadline: schedule(inner.mtm, grace, inner.this.clone(), deadline_fired),
                end: Box::new(end),
            });
            QuitReply::Later
        })
        .unwrap_or(QuitReply::Now)
    }

    /// Runs `f` on the state; `None` when it is busy, which only a nested
    /// call could cause.
    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> Option<T> {
        match self.inner.try_borrow_mut() {
            Ok(mut inner) => Some(f(&mut inner)),
            Err(_) => {
                warn!("the jobs indicator is busy; the update is dropped");
                None
            }
        }
    }
}

impl fmt::Debug for Jobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jobs").finish_non_exhaustive()
    }
}

impl Inner {
    fn cancel(&self, job: JobId) {
        self.sink.send(AppEvent::CancelJobRequested { job });
    }

    /// Shows what is due, then keeps one timer for the next hidden job.
    fn update(&mut self) {
        self.indicator.tick(Instant::now());
        self.view
            .show(&self.indicator.rows(), self.indicator.icon());
        let next = self.indicator.deadline();
        if self.timer.as_ref().map(|(at, _)| *at) == next {
            return;
        }
        if let Some((_, timer)) = self.timer.take() {
            timer.invalidate();
        }
        if let Some(at) = next {
            let delay = at.saturating_duration_since(Instant::now());
            let timer = schedule(self.mtm, delay, self.this.clone(), threshold_fired);
            self.timer = Some((at, timer));
        }
    }

    /// Stops a waiting Quit and hands back its callback.
    fn end_quit(&mut self) -> Option<Box<dyn FnOnce(QuitEnd)>> {
        let quit = self.quit.take()?;
        quit.deadline.invalidate();
        Some(quit.end)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some((_, timer)) = self.timer.take() {
            timer.invalidate();
        }
        if let Some(quit) = self.quit.take() {
            quit.deadline.invalidate();
        }
    }
}

fn threshold_fired(inner: &Rc<RefCell<Inner>>) {
    match inner.try_borrow_mut() {
        Ok(mut inner) => {
            // A timer that fired is done.
            inner.timer = None;
            inner.update();
        }
        Err(_) => warn!("the jobs indicator is busy when its timer fired"),
    }
}

fn deadline_fired(inner: &Rc<RefCell<Inner>>) {
    let end = match inner.try_borrow_mut() {
        Ok(mut inner) => inner.quit.take().map(|quit| quit.end),
        Err(_) => {
            warn!("the jobs indicator is busy when the quit deadline came");
            None
        }
    };
    if let Some(end) = end {
        warn!("quit deadline: jobs still run");
        end(QuitEnd::Deadline);
    }
}

/// A timer on the main run loop, in the common modes, that calls `fired`
/// with the state after `delay`, unless the state is gone.
fn schedule(
    mtm: MainThreadMarker,
    delay: Duration,
    inner: Weak<RefCell<Inner>>,
    fired: fn(&Rc<RefCell<Inner>>),
) -> Retained<NSTimer> {
    let inner = MainThreadBound::new(inner, mtm);
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let Some(mtm) = MainThreadMarker::new() else {
            warn!("a jobs timer fired off the main thread");
            return;
        };
        if let Some(inner) = inner.get(mtm).upgrade() {
            fired(&inner);
        }
    });
    // SAFETY: the block is `Send` and `Sync`: it captures a
    // `MainThreadBound` and a function pointer, and reaches the state only
    // on the main thread.
    let timer =
        unsafe { NSTimer::timerWithTimeInterval_repeats_block(delay.as_secs_f64(), false, &block) };
    // SAFETY: the timer is new and not scheduled anywhere; the mode is a
    // Foundation constant. The main run loop fires it on the main thread.
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    timer
}
