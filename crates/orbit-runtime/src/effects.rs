//! Effect execution.

use std::path::Path;
use std::sync::Arc;

use orbit_app::messages::{AppEvent, Effect, Notice, ResolveError, ShellAction};
use orbit_app::ports::{EventSink, ResolveRequest, ShellError};
use tracing::error;

use crate::jobs::JobRunner;
use crate::platform::Views;
use crate::resolver::ResolverThread;

/// What the effects act on.
pub struct Services {
    /// Reads targets.
    pub resolver: ResolverThread,
    /// Runs jobs.
    pub jobs: JobRunner,
    /// Sends events back to the controller.
    pub sink: Arc<dyn EventSink>,
    /// Shows things and acts on the system.
    pub views: Views,
}

/// Carries out one effect. Failures come back to the controller as events
/// through the sink, or to the user through [`Feedback`], never by calling
/// the controller directly.
///
/// [`Feedback`]: orbit_app::ports::Feedback
pub fn execute(effect: Effect, services: &mut Services) {
    let views = &mut services.views;
    match effect {
        Effect::ResolveTarget { gesture } => {
            let request = ResolveRequest { gesture };
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
        Effect::PresentMenu(model) => views.menu.present(&model),
        Effect::Highlight(highlight) => views.menu.highlight(highlight),
        Effect::HideMenu => views.menu.hide(),
        Effect::PresentParameters(model) => views.parameters.present(&model),
        Effect::ParametersInvalid { gesture, error } => {
            views.parameters.show_invalid(gesture, error);
        }
        Effect::HideParameters => views.parameters.hide(),
        Effect::JobProgress { job, progress } => views.feedback.job_progress(job, progress),
        Effect::Notify(notice) => views.feedback.notify(&notice),
        Effect::ShowInfo(target) => views.feedback.show_info(&target),
        Effect::WriteClipboard(path) => {
            if let Err(err) = views.shell.write_clipboard(&path) {
                shell_failed(views, ShellAction::CopyPath, &path, err);
            }
        }
        Effect::RevealFile(path) => {
            if let Err(err) = views.shell.reveal(&path) {
                shell_failed(views, ShellAction::Reveal, &path, err);
            }
        }
    }
}

/// Tells the user that a shell action failed. The controller is not told:
/// nothing in its state depends on the action.
fn shell_failed(views: &mut Views, action: ShellAction, path: &Path, err: ShellError) {
    error!(?action, path = %path.display(), %err, "shell action failed");
    views.feedback.notify(&Notice::ActionFailed {
        action,
        message: err.0,
    });
}

#[cfg(test)]
mod tests {
    use orbit_app::catalog::{MenuModel, menu_slots};
    use orbit_app::messages::{
        GestureId, JobError, JobId, JobOutput, JobSpec, ParameterError, ParametersKind,
        ParametersModel, Progress,
    };
    use orbit_app::ports::{JobContext, OperationExecutor, TargetResolver};
    use orbit_app::state::Highlight;
    use orbit_domain::geometry::{MenuGeometry, ScreenPoint};
    use orbit_domain::operation::{NameError, OperationSpec};
    use orbit_domain::target::FileTarget;

    use super::*;
    use crate::jobs::JobsConfig;
    use crate::platform::ResolverFactory;
    use crate::testing::{Call, ChannelSink, Journal, plain, views};

    const G: GestureId = GestureId(4);

    struct Resolves;

    impl TargetResolver for Resolves {
        fn resolve(&mut self, _: &ResolveRequest) -> Result<FileTarget, ResolveError> {
            Ok(plain("/tmp/Docs"))
        }
    }

    struct Zips;

    impl OperationExecutor for Zips {
        fn execute(&self, spec: &JobSpec, _: &JobContext) -> Result<JobOutput, JobError> {
            Ok(JobOutput {
                path: spec.target.path.with_extension("zip"),
            })
        }
    }

    fn services(journal: &Journal, shell_error: Option<&str>) -> (Services, ChannelSink) {
        let sink = ChannelSink::new();
        let resolver = ResolverThread::spawn(ResolverFactory::new(|| Resolves), sink.shared());
        let jobs = JobRunner::new(Arc::new(Zips), sink.shared(), JobsConfig::default());
        match (resolver, jobs) {
            (Ok(resolver), Ok(jobs)) => (
                Services {
                    resolver,
                    jobs,
                    sink: sink.shared(),
                    views: views(journal, shell_error),
                },
                sink,
            ),
            (Err(err), _) | (_, Err(err)) => panic!("services: {err}"),
        }
    }

    fn menu() -> MenuModel {
        let target = plain("/tmp/report.txt");
        MenuModel {
            gesture: G,
            center: ScreenPoint::new(10.0, 20.0),
            geometry: MenuGeometry::standard(),
            title: "report.txt".into(),
            slots: menu_slots(&target.facts),
        }
    }

    fn form() -> ParametersModel {
        ParametersModel {
            gesture: G,
            kind: ParametersKind::Rename {
                current: "report.txt".into(),
            },
        }
    }

    fn run(effect: Effect, shell_error: Option<&str>) -> Vec<Call> {
        let journal = Journal::new();
        let (mut services, sink) = services(&journal, shell_error);
        execute(effect, &mut services);
        sink.assert_empty();
        journal.take()
    }

    /// Each effect that shows something or acts on the system calls exactly
    /// one port; nothing else is touched and no event is sent.
    #[test]
    fn port_effects_call_exactly_one_method() {
        let target = plain("/tmp/report.txt");
        let error = ParameterError::Name(NameError::Empty);
        let notice = Notice::NoFileName;
        let cases = [
            (Effect::PresentMenu(menu()), Call::MenuPresent(menu())),
            (
                Effect::Highlight(Highlight::Active(2)),
                Call::MenuHighlight(Highlight::Active(2)),
            ),
            (Effect::HideMenu, Call::MenuHide),
            (Effect::PresentParameters(form()), Call::FormPresent(form())),
            (
                Effect::ParametersInvalid { gesture: G, error },
                Call::FormInvalid(G, error),
            ),
            (Effect::HideParameters, Call::FormHide),
            (
                Effect::JobProgress {
                    job: JobId::new(1),
                    progress: Progress::Indeterminate,
                },
                Call::JobProgress(JobId::new(1), Progress::Indeterminate),
            ),
            (Effect::Notify(notice.clone()), Call::Notify(notice)),
            (Effect::ShowInfo(target.clone()), Call::ShowInfo(target)),
            (
                Effect::WriteClipboard("/tmp/a".into()),
                Call::WriteClipboard("/tmp/a".into()),
            ),
            (
                Effect::RevealFile("/tmp/a".into()),
                Call::Reveal("/tmp/a".into()),
            ),
        ];
        for (effect, call) in cases {
            let label = format!("{effect:?}");
            assert_eq!(run(effect, None), vec![call], "{label}");
        }
    }

    /// A failed shell action is the one case of two calls: the shell, then
    /// the user is told. No event reaches the controller.
    #[test]
    fn failed_shell_action_is_shown_to_the_user() {
        let cases = [
            (
                Effect::WriteClipboard("/tmp/a".into()),
                Call::WriteClipboard("/tmp/a".into()),
                ShellAction::CopyPath,
            ),
            (
                Effect::RevealFile("/tmp/a".into()),
                Call::Reveal("/tmp/a".into()),
                ShellAction::Reveal,
            ),
        ];
        for (effect, shell_call, action) in cases {
            let label = format!("{effect:?}");
            assert_eq!(
                run(effect, Some("denied")),
                vec![
                    shell_call,
                    Call::Notify(Notice::ActionFailed {
                        action,
                        message: "denied".into()
                    })
                ],
                "{label}"
            );
        }
    }

    #[test]
    fn resolve_target_uses_the_thread_and_no_port() {
        let journal = Journal::new();
        let (mut services, sink) = services(&journal, None);
        execute(Effect::ResolveTarget { gesture: G }, &mut services);
        assert_eq!(
            sink.next(),
            AppEvent::TargetResolved {
                gesture: G,
                result: Ok(plain("/tmp/Docs")),
            }
        );
        sink.assert_empty();
        assert_eq!(journal.take(), vec![]);
    }

    #[test]
    fn resolve_target_with_a_stopped_thread_is_answered_through_the_sink() {
        let journal = Journal::new();
        let (mut services, sink) = services(&journal, None);
        services.resolver.shutdown();
        execute(Effect::ResolveTarget { gesture: G }, &mut services);
        assert_eq!(
            sink.next(),
            AppEvent::TargetResolved {
                gesture: G,
                result: Err(ResolveError::Failed("resolver stopped".into())),
            }
        );
        sink.assert_empty();
        assert_eq!(journal.take(), vec![]);
    }

    fn spec(id: JobId) -> JobSpec {
        let target = plain("/tmp/Docs");
        JobSpec {
            id,
            destination: OperationSpec::ArchiveZip
                .destination(target.path.file_name().unwrap_or_default()),
            target,
            operation: OperationSpec::ArchiveZip,
        }
    }

    #[test]
    fn start_job_runs_on_a_worker_and_finishes_through_the_sink() {
        let journal = Journal::new();
        let (mut services, sink) = services(&journal, None);
        execute(Effect::StartJob(spec(JobId::new(1))), &mut services);
        let AppEvent::JobFinished { job, result } = sink.next() else {
            panic!("not a finish");
        };
        assert_eq!(job, JobId::new(1));
        assert_eq!(
            result,
            Ok(JobOutput {
                path: "/tmp/Docs.zip".into()
            })
        );
        sink.assert_empty();
        assert_eq!(journal.take(), vec![]);
    }

    #[test]
    fn cancel_job_touches_no_port_and_sends_nothing() {
        let journal = Journal::new();
        let (mut services, sink) = services(&journal, None);
        execute(Effect::CancelJob(JobId::new(1)), &mut services);
        sink.assert_empty();
        assert_eq!(journal.take(), vec![]);
    }
}
