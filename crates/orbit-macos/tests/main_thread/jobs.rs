//! The jobs indicator and Quit on the real main run loop.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use objc2::{MainThreadMarker, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationTerminateReply, NSEventTrackingRunLoopMode, NSMenu,
    NSModalPanelRunLoopMode,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop, NSString};
use orbit_app::messages::{AppEvent, JobId, JobSpec, Progress};
use orbit_app::ports::EventSink;
use orbit_domain::operation::OperationSpec;
use orbit_domain::target::{FileFacts, FileIdentity, FileKind, FileTarget, FileVersion};
use orbit_macos::operations::children::Children;
use orbit_macos::ui::delegate::AppDelegate;
use orbit_macos::ui::jobs::{Jobs, JobsView, QuitEnd, QuitReply};
use orbit_macos::ui::jobs_indicator::{Icon, JobRow, SHOW_AFTER};
use orbit_macos::ui::status_item::StatusMenu;

const TIMEOUT: Duration = Duration::from_secs(5);
/// A short grace, so that the test does not wait the real two seconds.
const GRACE: Duration = Duration::from_millis(300);

pub(super) fn run(mtm: MainThreadMarker) -> Result<(), String> {
    // SAFETY: reading immutable framework constants.
    let (default, tracking, modal) = unsafe {
        (
            NSDefaultRunLoopMode,
            NSEventTrackingRunLoopMode,
            NSModalPanelRunLoopMode,
        )
    };
    shown_after_the_threshold(mtm, default, "default mode")?;
    shown_after_the_threshold(mtm, tracking, "event tracking mode")?;
    quit_ends_with_the_last_job(mtm, default)?;
    quit_ends_at_the_deadline(mtm, modal)?;
    quit_without_jobs_is_at_once(mtm)?;
    status_menu_rows(mtm)?;
    delegate_replies(mtm)?;
    Ok(())
}

/// Runs the main run loop in `mode` until `done`, or fails after
/// [`TIMEOUT`].
pub(super) fn pump_in(mode: &NSString, done: impl Fn() -> bool, what: &str) -> Result<(), String> {
    let started = Instant::now();
    while !done() {
        if started.elapsed() > TIMEOUT {
            return Err(format!("{what}: not done in {TIMEOUT:?}"));
        }
        pump_once(mode);
    }
    Ok(())
}

/// Runs the main run loop in `mode` for `period`.
fn pump_for(mode: &NSString, period: Duration) {
    let started = Instant::now();
    while started.elapsed() < period {
        pump_once(mode);
    }
}

fn pump_once(mode: &NSString) {
    let until = NSDate::dateWithTimeIntervalSinceNow(0.02);
    // Returns at once when the mode has no sources; the callers loop.
    NSRunLoop::currentRunLoop().runMode_beforeDate(mode, &until);
}

/// The last rows and icon shown.
type Shown = Rc<RefCell<(Vec<JobRow>, Icon)>>;

struct RecordingView(Shown);

impl JobsView for RecordingView {
    fn show(&mut self, rows: &[JobRow], icon: Icon) {
        *self.0.borrow_mut() = (rows.to_vec(), icon);
    }
}

#[derive(Default)]
struct RecordingSink(Mutex<Vec<AppEvent>>);

impl RecordingSink {
    fn cancelled(&self) -> Vec<u64> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|event| match event {
                AppEvent::CancelJobRequested { job } => Some(job.get()),
                _ => None,
            })
            .collect()
    }
}

impl EventSink for RecordingSink {
    fn send(&self, event: AppEvent) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }
}

fn jobs(mtm: MainThreadMarker) -> (Jobs, Shown, Arc<RecordingSink>) {
    let shown: Shown = Rc::new(RefCell::new((Vec::new(), Icon::Idle)));
    let sink = Arc::new(RecordingSink::default());
    let jobs = Jobs::new(
        mtm,
        Box::new(RecordingView(Rc::clone(&shown))),
        Arc::clone(&sink) as Arc<dyn EventSink>,
    );
    (jobs, shown, sink)
}

fn spec(id: u64) -> JobSpec {
    let path = Path::new("/tmp/Docs");
    JobSpec {
        id: JobId::new(id),
        target: FileTarget {
            path: path.to_owned(),
            identity: FileIdentity {
                device: 1,
                inode: id,
            },
            version: FileVersion {
                size: 0,
                modified: SystemTime::UNIX_EPOCH,
            },
            facts: FileFacts {
                kind: FileKind::Directory,
                image: None,
            },
        },
        operation: OperationSpec::ArchiveZip,
        destination: OperationSpec::ArchiveZip.destination(path.as_os_str()),
    }
}

/// A job that reports nothing after its start is shown by the timer alone,
/// in `mode`, not before the threshold.
fn shown_after_the_threshold(
    mtm: MainThreadMarker,
    mode: &NSString,
    name: &str,
) -> Result<(), String> {
    let (jobs, shown, _sink) = jobs(mtm);
    let start = Instant::now();
    jobs.started(&spec(1));
    jobs.progress(JobId::new(1), Progress::Indeterminate);
    if !shown.borrow().0.is_empty() {
        return Err(format!("{name}: the job is shown at once"));
    }
    pump_in(
        mode,
        || !shown.borrow().0.is_empty(),
        &format!("{name}: the threshold timer"),
    )?;
    let elapsed = start.elapsed();
    if elapsed < SHOW_AFTER {
        return Err(format!("{name}: shown after {elapsed:?}"));
    }
    if shown.borrow().1 != Icon::Busy {
        return Err(format!("{name}: icon {:?}", shown.borrow().1));
    }
    jobs.finished(JobId::new(1));
    if *shown.borrow() != (Vec::new(), Icon::Idle) {
        return Err(format!("{name}: the row stays after the result"));
    }
    println!("test jobs: threshold in {name} ... ok ({elapsed:?})");
    Ok(())
}

fn counter() -> (Rc<Cell<Option<QuitEnd>>>, Rc<Cell<u32>>) {
    (Rc::new(Cell::new(None)), Rc::new(Cell::new(0)))
}

/// Quit cancels every job, also one started while it waits, and ends once,
/// with the result of the last job.
fn quit_ends_with_the_last_job(mtm: MainThreadMarker, mode: &NSString) -> Result<(), String> {
    let (jobs, _shown, sink) = jobs(mtm);
    jobs.started(&spec(1));
    jobs.started(&spec(2));
    let (ended, calls) = counter();
    let (end, count) = (Rc::clone(&ended), Rc::clone(&calls));
    let reply = jobs.quit(Duration::from_secs(2), move |how| {
        end.set(Some(how));
        count.set(count.get() + 1);
    });
    if reply != QuitReply::Later || !jobs.is_quitting() {
        return Err(format!("quit with jobs: {reply:?}"));
    }
    jobs.started(&spec(3));
    if sink.cancelled() != [1, 2, 3] {
        return Err(format!("cancelled: {:?}", sink.cancelled()));
    }
    jobs.finished(JobId::new(1));
    jobs.finished(JobId::new(3));
    if ended.get().is_some() {
        return Err("quit ended with a job left".into());
    }
    jobs.finished(JobId::new(2));
    if ended.get() != Some(QuitEnd::Finished) || jobs.is_quitting() {
        return Err(format!("quit after the last job: {:?}", ended.get()));
    }
    // The deadline timer is gone: nothing more comes.
    pump_for(mode, GRACE + Duration::from_millis(200));
    if calls.get() != 1 {
        return Err(format!("quit ended {} times", calls.get()));
    }
    println!("test jobs: quit after the last job ... ok");
    Ok(())
}

/// A job that never ends: Quit ends at the deadline, in the mode AppKit
/// runs while it waits.
fn quit_ends_at_the_deadline(mtm: MainThreadMarker, mode: &NSString) -> Result<(), String> {
    let (jobs, _shown, sink) = jobs(mtm);
    jobs.started(&spec(1));
    let (ended, calls) = counter();
    let (end, count) = (Rc::clone(&ended), Rc::clone(&calls));
    let start = Instant::now();
    if jobs.quit(GRACE, move |how| {
        end.set(Some(how));
        count.set(count.get() + 1);
    }) != QuitReply::Later
    {
        return Err("quit with a job is not later".into());
    }
    if sink.cancelled() != [1] {
        return Err(format!("cancelled: {:?}", sink.cancelled()));
    }
    pump_in(mode, || ended.get().is_some(), "the quit deadline")?;
    let elapsed = start.elapsed();
    if ended.get() != Some(QuitEnd::Deadline) || elapsed < GRACE {
        return Err(format!("quit ended {:?} after {elapsed:?}", ended.get()));
    }
    // A late result ends nothing more.
    jobs.finished(JobId::new(1));
    if calls.get() != 1 {
        return Err(format!("quit ended {} times", calls.get()));
    }
    println!("test jobs: quit at the deadline ... ok ({elapsed:?})");
    Ok(())
}

fn quit_without_jobs_is_at_once(mtm: MainThreadMarker) -> Result<(), String> {
    let (jobs, _shown, sink) = jobs(mtm);
    let called = Rc::new(Cell::new(false));
    let flag = Rc::clone(&called);
    let reply = jobs.quit(GRACE, move |_| flag.set(true));
    if reply != QuitReply::Now || called.get() || !sink.cancelled().is_empty() {
        return Err(format!("quit without jobs: {reply:?}"));
    }
    println!("test jobs: quit without jobs ... ok");
    Ok(())
}

fn titles(menu: &NSMenu) -> Vec<String> {
    (0..menu.numberOfItems())
        .filter_map(|index| menu.itemAtIndex(index))
        .map(|item| item.title().to_string())
        .collect()
}

fn row(job: u64, text: &str) -> JobRow {
    JobRow {
        job: JobId::new(job),
        text: text.to_owned(),
    }
}

/// Rows go in and out of the real menu in place; Cancel sends the
/// cancellation of its job; the ring draws.
fn status_menu_rows(mtm: MainThreadMarker) -> Result<(), String> {
    let sink = Arc::new(RecordingSink::default());
    let mut status = StatusMenu::new(mtm, Arc::clone(&sink) as Arc<dyn EventSink>);
    let item = status.item();
    let menu = item.menu(mtm).ok_or("no menu")?;
    let button = item.button(mtm).ok_or("no button")?;
    if titles(&menu) != ["Quit Orbit"] || button.title().to_string() != "◎" {
        return Err(format!("idle menu: {:?}", titles(&menu)));
    }

    status.show(&[row(7, "Compressing “a” — waiting")], Icon::Busy);
    let label = menu.itemAtIndex(0).ok_or("no row")?;
    status.show(
        &[row(7, "Compressing “a” — 42%"), row(8, "Compressing “b”…")],
        Icon::Ring(42),
    );
    let expected = [
        "Compressing “a” — 42%",
        "Cancel",
        "Compressing “b”…",
        "Cancel",
        "",
        "Quit Orbit",
    ];
    if titles(&menu) != expected {
        return Err(format!("menu with two jobs: {:?}", titles(&menu)));
    }
    // The first row was retitled, not replaced.
    if menu.itemAtIndex(0).as_deref() != Some(&*label) {
        return Err("the row of the first job was rebuilt".into());
    }
    let image = button.image().ok_or("no ring")?;
    if !image.isTemplate() || image.TIFFRepresentation().is_none() {
        return Err("the ring is not a template image that draws".into());
    }

    menu.performActionForItemAtIndex(3);
    if sink.cancelled() != [8] {
        return Err(format!("cancel sent {:?}", sink.cancelled()));
    }

    status.show(&[row(8, "Compressing “b”…")], Icon::Busy);
    if titles(&menu) != ["Compressing “b”…", "Cancel", "", "Quit Orbit"] {
        return Err(format!("menu with one job: {:?}", titles(&menu)));
    }
    status.show(&[], Icon::Idle);
    if titles(&menu) != ["Quit Orbit"] || button.title().to_string() != "◎" {
        return Err(format!("menu after the jobs: {:?}", titles(&menu)));
    }
    println!("test jobs: status menu rows ... ok");
    Ok(())
}

/// The delegate answers Quit through the Objective-C runtime: at once
/// without jobs, later with one, which it cancels.
fn delegate_replies(mtm: MainThreadMarker) -> Result<(), String> {
    let app = NSApplication::sharedApplication(mtm);
    let ask = |delegate: &AppDelegate| -> NSApplicationTerminateReply {
        // SAFETY: the method of the delegate protocol, with the application.
        unsafe { msg_send![delegate, applicationShouldTerminate: &*app] }
    };
    let (jobs, _shown, sink) = jobs(mtm);
    let delegate = AppDelegate::new(mtm, jobs.clone(), Children::new());
    if ask(&delegate) != NSApplicationTerminateReply::TerminateNow {
        return Err("quit without jobs is not at once".into());
    }
    jobs.started(&spec(1));
    if ask(&delegate) != NSApplicationTerminateReply::TerminateLater || sink.cancelled() != [1] {
        return Err(format!("quit with a job: cancelled {:?}", sink.cancelled()));
    }
    // Dropping the jobs stops the deadline, which would quit this process.
    drop(delegate);
    drop(jobs);
    println!("test jobs: delegate replies ... ok");
    Ok(())
}
