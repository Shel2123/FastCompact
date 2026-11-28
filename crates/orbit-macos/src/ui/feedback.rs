//! What the user is told besides the menu: toasts and the info panel.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use objc2::MainThreadMarker;
use orbit_app::messages::{JobId, JobSpec, Notice, Progress};
use orbit_app::ports::Feedback;
use orbit_domain::target::FileTarget;
use tracing::{debug, error, info};

use super::backdrop::Background;
use super::info_close::CloseWatch;
use super::info_panel::InfoPanel;
use super::info_sheet::{MacFormats, info_sheet};
use super::info_size::SizeJob;
use super::jobs::Jobs;
use super::notice_text::notice_text;
use super::toast::Toast;

/// The [`Feedback`] of the macOS platform.
pub struct MacFeedback {
    mtm: MainThreadMarker,
    toast: Toast,
    /// The jobs in the menu bar, shared with Quit.
    jobs: Jobs,
    info: Rc<InfoPanel>,
    formats: Rc<MacFormats>,
    /// What closes the info panel while it is on screen.
    watch: Option<CloseWatch>,
    /// The count of the folder whose info is on screen, if any.
    size: Option<SizeJob>,
    /// Debug switch: each "show info" shows the panel twice in a row.
    info_twice: bool,
}

impl MacFeedback {
    /// Builds the windows, hidden; jobs are shown by `jobs`. `background`
    /// is the one asked for by the debug switch, if any; with `info_twice`
    /// (a debug switch too) the info panel is shown twice for each request,
    /// to see the second replace the first.
    pub fn new(
        mtm: MainThreadMarker,
        jobs: Jobs,
        background: Option<Background>,
        info_twice: bool,
    ) -> Self {
        let background = Background::available(background);
        Self {
            mtm,
            toast: Toast::new(mtm, background),
            jobs,
            info: Rc::new(InfoPanel::new(mtm, background)),
            formats: Rc::new(MacFormats::new()),
            watch: None,
            size: None,
            info_twice,
        }
    }

    /// Shows the info panel for `target`, replacing the one on screen. The
    /// panel is shown only with its frame found and its watch started; on
    /// any failure the panel on screen, if any, is hidden and its watch
    /// stopped, so no panel stays up with nothing to close it.
    fn show_info_panel(&mut self, target: &FileTarget) {
        let started = Instant::now();
        let sheet = info_sheet(target, self.formats.as_ref());
        let Some(frame) = self.info.prepare(&sheet) else {
            self.give_up("no display");
            return;
        };
        // Raised when this panel closes or is replaced, so that a count of
        // its folder stops and writes nothing more.
        let cancel = Arc::new(AtomicBool::new(false));
        let panel = Rc::downgrade(&self.info);
        let closed = Arc::clone(&cancel);
        let Some(watch) = CloseWatch::start(self.mtm, move || {
            closed.store(true, Ordering::Relaxed);
            if let Some(panel) = panel.upgrade() {
                panel.hide();
            }
            info!("info panel closed");
        }) else {
            self.give_up("its event monitors cannot be added");
            return;
        };
        self.info.show(frame);
        // The old watch and count stop as they are dropped: neither touches
        // the new panel.
        self.watch = Some(watch);
        self.size = sheet.counting.then(|| {
            SizeJob::start(
                self.mtm,
                target.path.clone(),
                cancel,
                Rc::downgrade(&self.info),
                Rc::clone(&self.formats),
            )
        });
        // Layout, monitors and ordering the window in; not the first frame
        // on screen, which is drawn later.
        debug!(elapsed = ?started.elapsed(), "info panel shown");
    }

    fn give_up(&mut self, reason: &str) {
        error!(reason, "the info panel is not shown");
        self.watch = None;
        self.size = None;
        self.info.hide();
    }
}

impl Feedback for MacFeedback {
    fn job_started(&mut self, spec: &JobSpec) {
        info!(job = spec.id.get(), operation = ?spec.operation, "job started");
        self.jobs.started(spec);
    }

    fn job_progress(&mut self, job: JobId, progress: Progress) {
        debug!(job = job.get(), ?progress, "job progress");
        self.jobs.progress(job, progress);
    }

    fn notify(&mut self, notice: &Notice) {
        info!(?notice, "notify");
        let finished = match notice {
            Notice::JobSucceeded { job, .. } | Notice::JobFailed { job, .. } => Some(*job),
            _ => None,
        };
        // While Quit waits, the results of the cancelled jobs would flash
        // up just before the application goes.
        if self.jobs.is_quitting() {
            debug!("quitting; no toast");
        } else {
            self.toast.show(&notice_text(notice));
        }
        // Last: the end of the last job may quit the application.
        if let Some(job) = finished {
            self.jobs.finished(job);
        }
    }

    fn show_info(&mut self, target: &FileTarget) {
        info!(?target, "show info");
        self.show_info_panel(target);
        if self.info_twice {
            info!("debug: info shown again");
            self.show_info_panel(target);
        }
    }
}
