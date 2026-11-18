//! Stand-ins that only log what they would show: the menu when the radial
//! menu cannot be built (a degraded mode), the form until stages 8 and 9,
//! and the shell until step 4 of stage 6.

use std::path::Path;

use orbit_app::catalog::MenuModel;
use orbit_app::messages::{GestureId, ParameterError, ParametersModel};
use orbit_app::ports::{MenuView, ParametersView, Shell, ShellError};
use orbit_app::state::Highlight;
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
