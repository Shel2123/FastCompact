//! Effect execution.

use std::sync::Arc;

use orbit_app::messages::{AppEvent, Effect, ResolveError};
use orbit_app::ports::{EventSink, ResolveRequest};
use tracing::{error, info};

use super::jobs::JobRunner;
use super::resolver::ResolverThread;

/// What the effects act on.
pub struct Services {
    /// Reads targets.
    pub resolver: ResolverThread,
    /// Runs jobs.
    pub jobs: JobRunner,
    /// Sends events back to the controller.
    pub sink: Arc<dyn EventSink>,
}

/// Carries out one effect. Failures come back to the controller as events
/// through the sink, never by calling it directly.
pub fn execute(effect: Effect, services: &mut Services) {
    match effect {
        Effect::ResolveTarget {
            gesture,
            pressed_at,
        } => {
            let request = ResolveRequest {
                gesture,
                pressed_at,
            };
            if services.resolver.request(request).is_err() {
                error!(?gesture, "resolver thread is stopped");
                services.sink.send(AppEvent::TargetResolved {
                    gesture,
                    result: Err(ResolveError::Failed("resolver stopped".into())),
                });
            }
        }
        Effect::StartJob(spec) => services.jobs.start(spec),
        Effect::CancelJob(job) => services.jobs.cancel(job),
        // Shown by the UI from stage 5 on.
        Effect::PresentMenu(model) => info!(?model, "present menu"),
        Effect::Highlight(highlight) => info!(?highlight, "highlight"),
        Effect::HideMenu => info!("hide menu"),
        Effect::PresentParameters(model) => info!(?model, "present parameters"),
        Effect::ParametersInvalid { gesture, error } => {
            info!(?gesture, %error, "parameters invalid");
        }
        Effect::HideParameters => info!("hide parameters"),
        Effect::JobProgress { job, progress } => {
            info!(job = job.get(), ?progress, "job progress");
        }
        Effect::Notify(notice) => info!(?notice, "notify"),
        // Carried out from stage 6 on.
        Effect::WriteClipboard(path) => info!(path = %path.display(), "write clipboard"),
        Effect::RevealFile(path) => info!(path = %path.display(), "reveal file"),
        Effect::ShowInfo(target) => info!(?target, "show info"),
    }
}
