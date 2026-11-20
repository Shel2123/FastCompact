//! What the user is told besides the menu: toasts and, from step 5 of
//! stage 6, the info panel. For now (step 5a) "show info" only starts the
//! watch that will close the panel and logs what it would do.

use objc2::MainThreadMarker;
use orbit_app::messages::{JobId, Notice, Progress};
use orbit_app::ports::Feedback;
use orbit_domain::target::FileTarget;
use tracing::info;

use super::backdrop::Background;
use super::info_close::CloseWatch;
use super::notice_text::notice_text;
use super::toast::Toast;

/// The [`Feedback`] of the macOS platform.
pub struct MacFeedback {
    mtm: MainThreadMarker,
    toast: Toast,
    /// The watch of the info panel on screen, if any.
    info: Option<CloseWatch>,
}

impl MacFeedback {
    /// Builds the windows, hidden. `background` is the one asked for by the
    /// debug switch, if any.
    pub fn new(mtm: MainThreadMarker, background: Option<Background>) -> Self {
        Self {
            mtm,
            toast: Toast::new(mtm, Background::available(background)),
            info: None,
        }
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
        // A new watch replaces the old one, which stops as it is dropped.
        self.info = Some(CloseWatch::start(self.mtm, || {
            info!("info panel would close");
        }));
    }
}
