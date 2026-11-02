//! Stand-ins for the ports that later stages implement: the menu and the
//! form (stage 5), the clipboard, Reveal and info (stage 6). They only log
//! what they would show.

use std::path::Path;

use orbit_app::catalog::MenuModel;
use orbit_app::messages::{GestureId, JobId, Notice, ParameterError, ParametersModel, Progress};
use orbit_app::ports::{Feedback, MenuView, ParametersView, Shell, ShellError};
use orbit_app::state::Highlight;
use orbit_domain::target::FileTarget;
use tracing::info;

/// Logs the menu.
#[derive(Debug, Default)]
pub struct LoggingMenuView;

impl MenuView for LoggingMenuView {
    fn present(&mut self, model: &MenuModel) {
        info!(?model, "present menu");
    }

    fn highlight(&mut self, highlight: Highlight) {
        info!(?highlight, "highlight");
    }

    fn hide(&mut self) {
        info!("hide menu");
    }
}

/// Logs the form.
#[derive(Debug, Default)]
pub struct LoggingParametersView;

impl ParametersView for LoggingParametersView {
    fn present(&mut self, model: &ParametersModel) {
        info!(?model, "present parameters");
    }

    fn show_invalid(&mut self, gesture: GestureId, error: ParameterError) {
        info!(?gesture, %error, "parameters invalid");
    }

    fn hide(&mut self) {
        info!("hide parameters");
    }
}

/// Logs progress, notices and information.
#[derive(Debug, Default)]
pub struct LoggingFeedback;

impl Feedback for LoggingFeedback {
    fn job_progress(&mut self, job: JobId, progress: Progress) {
        info!(job = job.get(), ?progress, "job progress");
    }

    fn notify(&mut self, notice: &Notice) {
        info!(?notice, "notify");
    }

    fn show_info(&mut self, target: &FileTarget) {
        info!(?target, "show info");
    }
}

/// Logs the clipboard and Reveal.
#[derive(Debug, Default)]
pub struct LoggingShell;

impl Shell for LoggingShell {
    fn write_clipboard(&mut self, path: &Path) -> Result<(), ShellError> {
        info!(path = %path.display(), "write clipboard");
        Ok(())
    }

    fn reveal(&mut self, path: &Path) -> Result<(), ShellError> {
        info!(path = %path.display(), "reveal file");
        Ok(())
    }
}
