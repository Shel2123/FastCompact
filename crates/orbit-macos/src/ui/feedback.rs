//! What the user is told besides the menu: toasts and the info panel.

use std::rc::Rc;
use std::time::Instant;

use objc2::MainThreadMarker;
use orbit_app::messages::{JobId, Notice, Progress};
use orbit_app::ports::Feedback;
use orbit_domain::target::FileTarget;
use tracing::{debug, error, info};

use super::backdrop::Background;
use super::info_close::CloseWatch;
use super::info_panel::InfoPanel;
use super::info_sheet::{MacFormats, info_sheet};
use super::notice_text::notice_text;
use super::toast::Toast;

/// The [`Feedback`] of the macOS platform.
pub struct MacFeedback {
    mtm: MainThreadMarker,
    toast: Toast,
    info: Rc<InfoPanel>,
    formats: MacFormats,
    /// What closes the info panel while it is on screen.
    watch: Option<CloseWatch>,
    /// Debug switch: each "show info" shows the panel twice in a row.
    info_twice: bool,
}

impl MacFeedback {
    /// Builds the windows, hidden. `background` is the one asked for by the
    /// debug switch, if any; with `info_twice` (a debug switch too) the info
    /// panel is shown twice for each request, to see the second replace the
    /// first.
    pub fn new(mtm: MainThreadMarker, background: Option<Background>, info_twice: bool) -> Self {
        let background = Background::available(background);
        Self {
            mtm,
            toast: Toast::new(mtm, background),
            info: Rc::new(InfoPanel::new(mtm, background)),
            formats: MacFormats::new(),
            watch: None,
            info_twice,
        }
    }

    /// Shows the info panel for `target`, replacing the one on screen. The
    /// panel is shown only with its frame found and its watch started; on
    /// any failure the panel on screen, if any, is hidden and its watch
    /// stopped, so no panel stays up with nothing to close it.
    fn show_info_panel(&mut self, target: &FileTarget) {
        let started = Instant::now();
        let sheet = info_sheet(target, &self.formats);
        let Some(frame) = self.info.prepare(&sheet) else {
            self.give_up("no display");
            return;
        };
        let panel = Rc::downgrade(&self.info);
        let Some(watch) = CloseWatch::start(self.mtm, move || {
            if let Some(panel) = panel.upgrade() {
                panel.hide();
            }
            info!("info panel closed");
        }) else {
            self.give_up("its event monitors cannot be added");
            return;
        };
        self.info.show(frame);
        // The old watch stops as it is dropped: it never hides the new panel.
        self.watch = Some(watch);
        // Layout, monitors and ordering the window in; not the first frame
        // on screen, which is drawn later.
        debug!(elapsed = ?started.elapsed(), "info panel shown");
    }

    fn give_up(&mut self, reason: &str) {
        error!(reason, "the info panel is not shown");
        self.watch = None;
        self.info.hide();
    }
}

impl Feedback for MacFeedback {
    fn job_progress(&mut self, job: JobId, progress: Progress) {
        // Shown from stage 7, with the jobs.
        info!(job = job.get(), ?progress, "job progress");
    }

    fn notify(&mut self, notice: &Notice) {
        info!(?notice, "notify");
        self.toast.show(&notice_text(notice));
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
