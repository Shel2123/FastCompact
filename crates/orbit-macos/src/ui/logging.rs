//! Stand-ins that only log what they would show: the menu when the radial
//! menu cannot be built (a degraded mode) and the form until stages 8
//! and 9.

use orbit_app::catalog::MenuModel;
use orbit_app::messages::{GestureId, ParameterError, ParametersModel};
use orbit_app::ports::{MenuView, ParametersView};
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
