//! What a toast says for each [`Notice`].

use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use orbit_app::messages::{JobError, Notice, ResolveError, ShellAction};
use orbit_domain::operation::OperationSpec;

/// How long a toast of good news stays.
pub const SUCCESS_DURATION: Duration = Duration::from_millis(1500);
/// How long a toast about a failure stays: long enough to read the reason.
pub const FAILURE_DURATION: Duration = Duration::from_secs(4);

/// Whether a toast is good news.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Something worked.
    Success,
    /// Something did not happen.
    Failure,
    /// Something stopped because the user asked.
    Neutral,
}

/// What a toast shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastText {
    /// SF Symbol in front of the text.
    pub symbol: &'static str,
    /// One line for the user.
    pub text: String,
    /// Good news or not; decides the color of the symbol.
    pub tone: Tone,
    /// How long the toast stays.
    pub duration: Duration,
}

impl ToastText {
    fn success(symbol: &'static str, text: String) -> Self {
        Self {
            symbol,
            text,
            tone: Tone::Success,
            duration: SUCCESS_DURATION,
        }
    }

    fn neutral(symbol: &'static str, text: String) -> Self {
        Self {
            symbol,
            text,
            tone: Tone::Neutral,
            duration: SUCCESS_DURATION,
        }
    }

    fn failure(text: String) -> Self {
        Self {
            symbol: "exclamationmark.triangle",
            text,
            tone: Tone::Failure,
            duration: FAILURE_DURATION,
        }
    }
}

/// The toast for `notice`.
pub fn notice_text(notice: &Notice) -> ToastText {
    match notice {
        Notice::Refused(error) => ToastText::failure(refusal(error)),
        Notice::NoFileName => ToastText::failure("This item has no name to act on".into()),
        Notice::PathCopied => ToastText::success("doc.on.clipboard", "Path copied".into()),
        Notice::JobSucceeded {
            operation, output, ..
        } => ToastText::success(
            "checkmark.circle",
            format!("{}: {}", done(operation), file_name(&output.path)),
        ),
        Notice::JobFailed {
            operation, error, ..
        } => job_failed(operation, error),
        Notice::ActionFailed { action, message } => {
            let what = match action {
                ShellAction::CopyPath => "Couldn't copy the path",
                ShellAction::Reveal => "Couldn't show the item in Finder",
            };
            ToastText::failure(format!("{what}: {message}"))
        }
    }
}

/// A job that did not produce its result.
fn job_failed(operation: &OperationSpec, error: &JobError) -> ToastText {
    match error {
        // The user asked for it: not a failure.
        JobError::Cancelled => {
            ToastText::neutral("xmark.circle", format!("{} cancelled", title(operation)))
        }
        // Mostly a disk the result can't be published on safely, with a
        // sentence of its own. Any other unsupported call still names what
        // failed in the message (“couldn't read “a”: …”).
        JobError::Io {
            kind: ErrorKind::Unsupported,
            message,
        } => ToastText::failure(message.clone()),
        error => ToastText::failure(format!("{} failed: {error}", title(operation))),
    }
}

/// Why a gesture was refused, as the user can act on it.
fn refusal(error: &ResolveError) -> String {
    match error {
        ResolveError::FileManagerNotActive => "Finder is not the active app".into(),
        ResolveError::OutsideFileArea => {
            "Select an item in a Finder window or on the desktop".into()
        }
        ResolveError::NothingSelected => "Nothing is selected".into(),
        ResolveError::MultipleSelected => "Select only one item".into(),
        ResolveError::SelectionUnstable => "The selection changed; try again".into(),
        ResolveError::AccessibilityDenied => "Orbit needs Accessibility access".into(),
        ResolveError::InputMonitoringDenied => "Orbit needs Input Monitoring access".into(),
        ResolveError::SecureInputOn => "Secure keyboard entry is on".into(),
        ResolveError::Failed(message) => format!("Couldn't read the selection: {message}"),
    }
}

/// The operation as a title, for a failure.
fn title(operation: &OperationSpec) -> &'static str {
    match operation {
        OperationSpec::ArchiveZip => "ZIP",
        OperationSpec::ConvertImage { .. } => "Conversion",
        OperationSpec::ResizeImage { .. } => "Resize",
        OperationSpec::Rename { .. } => "Rename",
    }
}

/// The operation as done, before the name of its result.
fn done(operation: &OperationSpec) -> &'static str {
    match operation {
        OperationSpec::ArchiveZip
        | OperationSpec::ConvertImage { .. }
        | OperationSpec::ResizeImage { .. } => "Created",
        OperationSpec::Rename { .. } => "Renamed to",
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io;

    use orbit_app::messages::{JobId, JobOutput};

    use super::*;

    fn all_refusals() -> Vec<ResolveError> {
        // The match makes a new variant fail to compile here until it is
        // listed.
        let listed = |error: &ResolveError| match error {
            ResolveError::FileManagerNotActive
            | ResolveError::OutsideFileArea
            | ResolveError::NothingSelected
            | ResolveError::MultipleSelected
            | ResolveError::SelectionUnstable
            | ResolveError::AccessibilityDenied
            | ResolveError::InputMonitoringDenied
            | ResolveError::SecureInputOn
            | ResolveError::Failed(_) => true,
        };
        let all = vec![
            ResolveError::FileManagerNotActive,
            ResolveError::OutsideFileArea,
            ResolveError::NothingSelected,
            ResolveError::MultipleSelected,
            ResolveError::SelectionUnstable,
            ResolveError::AccessibilityDenied,
            ResolveError::InputMonitoringDenied,
            ResolveError::SecureInputOn,
            ResolveError::Failed("AX timeout".into()),
        ];
        assert!(all.iter().all(listed));
        all
    }

    #[test]
    fn every_refusal_is_a_failure_with_its_own_text() {
        let mut texts = HashSet::new();
        for error in all_refusals() {
            let toast = notice_text(&Notice::Refused(error.clone()));
            assert_eq!(toast.tone, Tone::Failure, "{error:?}");
            assert_eq!(toast.duration, FAILURE_DURATION, "{error:?}");
            assert!(!toast.text.is_empty(), "{error:?}");
            assert!(texts.insert(toast.text), "{error:?} repeats a text");
        }
    }

    #[test]
    fn a_platform_failure_shows_its_message() {
        let toast = notice_text(&Notice::Refused(ResolveError::Failed("AX timeout".into())));
        assert_eq!(toast.text, "Couldn't read the selection: AX timeout");
    }

    #[test]
    fn copied_path_is_short_good_news() {
        assert_eq!(
            notice_text(&Notice::PathCopied),
            ToastText {
                symbol: "doc.on.clipboard",
                text: "Path copied".into(),
                tone: Tone::Success,
                duration: SUCCESS_DURATION,
            }
        );
    }

    #[test]
    fn no_file_name_is_a_failure() {
        let toast = notice_text(&Notice::NoFileName);
        assert_eq!(toast.tone, Tone::Failure);
        assert_eq!(toast.text, "This item has no name to act on");
    }

    #[test]
    fn failed_shell_actions_name_the_action_and_the_reason() {
        let copy = notice_text(&Notice::ActionFailed {
            action: ShellAction::CopyPath,
            message: "pasteboard busy".into(),
        });
        assert_eq!(copy.text, "Couldn't copy the path: pasteboard busy");
        assert_eq!(copy.tone, Tone::Failure);
        let reveal = notice_text(&Notice::ActionFailed {
            action: ShellAction::Reveal,
            message: "no such file".into(),
        });
        assert_eq!(
            reveal.text,
            "Couldn't show the item in Finder: no such file"
        );
        assert_eq!(reveal.duration, FAILURE_DURATION);
    }

    #[test]
    fn finished_jobs_name_their_result() {
        let toast = notice_text(&Notice::JobSucceeded {
            job: JobId::new(1),
            operation: OperationSpec::ArchiveZip,
            output: JobOutput {
                path: "/Users/me/Docs.zip".into(),
            },
        });
        assert_eq!(toast.text, "Created: Docs.zip");
        assert_eq!(toast.tone, Tone::Success);
    }

    #[test]
    fn failed_jobs_name_the_operation_and_the_error() {
        let toast = notice_text(&Notice::JobFailed {
            job: JobId::new(1),
            operation: OperationSpec::ArchiveZip,
            error: JobError::Io {
                kind: io::ErrorKind::PermissionDenied,
                message: "permission denied".into(),
            },
        });
        assert_eq!(toast.text, "ZIP failed: permission denied");
        assert_eq!(toast.tone, Tone::Failure);
    }

    fn failed_zip(error: JobError) -> ToastText {
        notice_text(&Notice::JobFailed {
            job: JobId::new(1),
            operation: OperationSpec::ArchiveZip,
            error,
        })
    }

    #[test]
    fn a_cancelled_job_is_neither_good_news_nor_a_failure() {
        assert_eq!(
            failed_zip(JobError::Cancelled),
            ToastText {
                symbol: "xmark.circle",
                text: "ZIP cancelled".into(),
                tone: Tone::Neutral,
                duration: SUCCESS_DURATION,
            }
        );
    }

    #[test]
    fn a_disk_without_safe_publishing_is_told_without_a_prefix() {
        let toast = failed_zip(JobError::Io {
            kind: io::ErrorKind::Unsupported,
            message: "Orbit can't safely create a ZIP on this disk".into(),
        });
        assert_eq!(toast.text, "Orbit can't safely create a ZIP on this disk");
        assert_eq!(toast.tone, Tone::Failure);
        assert_eq!(toast.duration, FAILURE_DURATION);
    }
}
