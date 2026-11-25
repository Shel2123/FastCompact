//! Counting the size of a folder for the info panel.
//!
//! The count runs on a thread of its own and writes into the Size row on
//! the main thread: a running sum every [`UPDATE_EVERY`], then the result.
//! A folder counted within that time shows its result at once.
//!
//! The job ends when the panel closes or is replaced: `cancel` is raised on
//! the main thread, and every write checks it there first, so a late result
//! never lands in a later panel. The walk stops at its next entry; a read
//! that blocks (a slow network volume) ends first.

use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::MainThreadMarker;
use tracing::{debug, warn};

use super::info_panel::InfoPanel;
use super::info_sheet::{MacFormats, SIZE_ROW, SizeState, size_text};
use crate::platform::folder_size::{Walk, folder_size};

/// How often the running sum is shown.
const UPDATE_EVERY: Duration = Duration::from_millis(250);

/// Where the count is written: main-thread objects, reached from the
/// counting thread only through the main queue.
type Target = MainThreadBound<(Weak<InfoPanel>, Rc<MacFormats>)>;

/// A count in progress; dropping it cancels the count.
pub(super) struct SizeJob {
    cancel: Arc<AtomicBool>,
}

impl SizeJob {
    /// Starts counting the folder at `path` for the Size row of `panel`.
    /// `cancel` may be raised by others too (the panel closing); the job
    /// raises it when dropped.
    pub(super) fn start(
        mtm: MainThreadMarker,
        path: PathBuf,
        cancel: Arc<AtomicBool>,
        panel: Weak<InfoPanel>,
        formats: Rc<MacFormats>,
    ) -> Self {
        let target = Arc::new(MainThreadBound::new((panel, formats), mtm));
        let worker = {
            let target = Arc::clone(&target);
            let cancel = Arc::clone(&cancel);
            move || count(&path, cancel, target)
        };
        let spawned = thread::Builder::new()
            .name("orbit-folder-size".into())
            .spawn(worker);
        if let Err(err) = spawned {
            warn!(%err, "no thread to count the folder size");
            write(mtm, &target, &cancel, "—");
        }
        Self { cancel }
    }
}

impl Drop for SizeJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The body of the counting thread.
fn count(path: &Path, cancel: Arc<AtomicBool>, target: Arc<Target>) {
    let started = Instant::now();
    let mut next = started + UPDATE_EVERY;
    let walk = folder_size(path, &cancel, |bytes| {
        let now = Instant::now();
        if now >= next {
            next = now + UPDATE_EVERY;
            deliver(
                Arc::clone(&target),
                Arc::clone(&cancel),
                SizeState::Counting(Some(bytes)),
            );
        }
    });
    match walk {
        Walk::Done(size) => {
            debug!(
                elapsed = ?started.elapsed(),
                bytes = size.bytes,
                unreadable = size.unreadable,
                "folder size counted"
            );
            deliver(target, cancel, SizeState::Done(size));
        }
        Walk::Cancelled => {
            debug!(elapsed = ?started.elapsed(), "folder size cancelled");
            // The last reference goes to the main thread too: dropping a
            // `MainThreadBound` elsewhere would wait for the main thread.
            DispatchQueue::main().exec_async(move || drop(target));
        }
    }
}

/// Writes `state` into the Size row on the main thread, unless the job was
/// cancelled by then.
fn deliver(target: Arc<Target>, cancel: Arc<AtomicBool>, state: SizeState) {
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            warn!("the folder size is delivered off the main thread");
            return;
        };
        let (_, formats) = target.get(mtm);
        let text = size_text(&state, formats.as_ref());
        write(mtm, &target, &cancel, &text);
    });
}

fn write(mtm: MainThreadMarker, target: &Target, cancel: &AtomicBool, text: &str) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let (panel, _) = target.get(mtm);
    if let Some(panel) = panel.upgrade() {
        panel.set_fact(SIZE_ROW, text);
    }
}
