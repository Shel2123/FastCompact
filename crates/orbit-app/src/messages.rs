//! Application events and effects.
//!
//! # Gesture events
//!
//! Every press of the hotkey is one gesture with its own [`GestureId`],
//! issued by the hotkey adapter on the main thread: a counter that grows only
//! when the key goes down. A repeated press while the key is held keeps the
//! id. Every event of a gesture carries the id of its press, so an event left
//! over from an earlier gesture is told apart from the current one even if it
//! arrives late. The same id names the whole scenario: the target request,
//! the menu and the parameters form.

use std::io;
use std::path::PathBuf;
use std::time::Instant;

use orbit_domain::geometry::{ScreenPoint, ScreenRect};
use orbit_domain::operation::{Destination, NameError, OperationSpec, ResizeBound, ResizeError};
use orbit_domain::target::{FileTarget, PixelSize, SourceChanged};
use thiserror::Error;

use crate::catalog::MenuModel;
use crate::state::Highlight;

/// One press of the hotkey, from key down to key up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GestureId(pub u64);

/// A background job; issued by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(u64);

impl JobId {
    /// The first id the controller issues.
    pub(crate) const FIRST: Self = Self(1);

    /// The id after this one. Ids only need to differ among running jobs,
    /// so wrapping around is harmless.
    pub(crate) const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    /// The raw number, for logs.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Something that happened, sent to the controller on the main thread.
#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    /// The hotkey was pressed, or a press repeated while it is held.
    Invoked {
        /// The press.
        gesture: GestureId,
        /// Cursor position read in the hotkey callback (I1).
        point: ScreenPoint,
        /// Visible frames of all displays.
        screens: Vec<ScreenRect>,
        /// When the key went down, from the event tap; the target resolver
        /// guards the selection against input after this moment.
        pressed_at: Instant,
    },
    /// The cursor moved during a gesture.
    PointerMoved {
        /// The press.
        gesture: GestureId,
        /// New cursor position.
        point: ScreenPoint,
    },
    /// The gesture ended: Space or one of the hotkey modifiers was released.
    Released {
        /// The press.
        gesture: GestureId,
        /// Cursor position at the release.
        point: ScreenPoint,
    },
    /// Esc during a gesture, while the target is resolved or the menu is
    /// open. Esc in the parameters form is
    /// [`AppEvent::ParametersCancelled`].
    GestureCancelled {
        /// The press.
        gesture: GestureId,
    },
    /// The target resolver answered a [`Effect::ResolveTarget`].
    TargetResolved {
        /// The press the request belongs to.
        gesture: GestureId,
        /// The target, or why there is none.
        result: Result<FileTarget, ResolveError>,
    },
    /// The parameters form was confirmed.
    ParametersSubmitted {
        /// The press that opened the form.
        gesture: GestureId,
        /// What the user entered.
        input: ParameterInput,
    },
    /// The parameters form was closed with Esc or its close button.
    ParametersCancelled {
        /// The press that opened the form.
        gesture: GestureId,
    },
    /// A job reported progress.
    JobProgress {
        /// The job.
        job: JobId,
        /// Its progress.
        progress: Progress,
    },
    /// A job finished. Delivered for every job that was started, including
    /// one the runtime could not queue.
    JobFinished {
        /// The job.
        job: JobId,
        /// What it produced, or why it failed.
        result: Result<JobOutput, JobError>,
    },
    /// The user asked to cancel a job.
    CancelJobRequested {
        /// The job.
        job: JobId,
    },
}

/// Something the adapters must do, returned by the controller in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Read the target of a gesture.
    ResolveTarget {
        /// The press; the answer must carry it.
        gesture: GestureId,
        /// When the key went down, for the selection guard.
        pressed_at: Instant,
    },
    /// Show the radial menu.
    PresentMenu(MenuModel),
    /// Change the highlighted slot of the open menu.
    Highlight(Highlight),
    /// Close the radial menu.
    HideMenu,
    /// Show the parameters form.
    PresentParameters(ParametersModel),
    /// Keep the form open and show why the input cannot be used.
    ParametersInvalid {
        /// The press that opened the form.
        gesture: GestureId,
        /// What is wrong.
        error: ParameterError,
    },
    /// Close the parameters form.
    HideParameters,
    /// Put the path on the clipboard.
    WriteClipboard(PathBuf),
    /// Show the object in Finder.
    RevealFile(PathBuf),
    /// Show information about the target.
    ShowInfo(FileTarget),
    /// Queue a job.
    StartJob(JobSpec),
    /// Cancel a running job.
    CancelJob(JobId),
    /// Show the progress of a job.
    JobProgress {
        /// The job.
        job: JobId,
        /// Its progress.
        progress: Progress,
    },
    /// Tell the user something.
    Notify(Notice),
}

/// Why the target of a gesture could not be read. Nothing is done to any
/// file in that case.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResolveError {
    /// Finder is not the active application.
    #[error("Finder is not the active application")]
    FinderNotFrontmost,
    /// Focus is outside the file area, such as in the sidebar or a preview.
    #[error("focus is outside the file area of Finder")]
    OutsideFileArea,
    /// Nothing is selected.
    #[error("nothing is selected")]
    NothingSelected,
    /// More than one object is selected.
    #[error("more than one object is selected")]
    MultipleSelected,
    /// The selection may have changed while it was read.
    #[error("the selection changed while it was read")]
    SelectionUnstable,
    /// Accessibility access is not granted.
    #[error("Accessibility access is not granted")]
    AccessibilityDenied,
    /// Input Monitoring access is not granted; the selection guard needs it.
    #[error("Input Monitoring access is not granted")]
    InputMonitoringDenied,
    /// Any other failure of the platform.
    #[error("the target could not be read: {0}")]
    Failed(String),
}

/// What the user entered in the parameters form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParameterInput {
    /// The new name, as typed.
    Rename(String),
    /// The side to fix and its new length.
    Resize(ResizeBound),
}

/// Why the input of the parameters form cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ParameterError {
    /// The name breaks a naming rule.
    #[error(transparent)]
    Name(#[from] NameError),
    /// The size cannot be computed.
    #[error(transparent)]
    Resize(#[from] ResizeError),
    /// Resize only makes images smaller.
    #[error("the new size must be smaller than the image")]
    NotSmaller,
}

/// Contents of the parameters form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParametersModel {
    /// The press that opened the form; the answer must carry it.
    pub gesture: GestureId,
    /// What the form asks for.
    pub kind: ParametersKind,
}

/// What a parameters form asks for, with its starting values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParametersKind {
    /// A new name; starts with the current one.
    Rename {
        /// Current file name, lossily converted for editing.
        current: String,
    },
    /// A smaller size; starts from the current one.
    Resize {
        /// Current size as displayed.
        size: PixelSize,
    },
}

/// A job to run in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// The job.
    pub id: JobId,
    /// The object it works on, as read when the gesture started.
    pub target: FileTarget,
    /// What it does.
    pub operation: OperationSpec,
    /// Name of the result in the folder of the target.
    pub destination: Destination,
}

/// What a finished job produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOutput {
    /// The created or renamed object.
    pub path: PathBuf,
}

/// Why a job failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JobError {
    /// The job queue was full; the job never ran.
    #[error("too many jobs are running")]
    QueueFull,
    /// The job was cancelled.
    #[error("the job was cancelled")]
    Cancelled,
    /// The source changed after the gesture.
    #[error(transparent)]
    SourceChanged(#[from] SourceChanged),
    /// The exact name of a rename is taken.
    #[error("the name is taken")]
    NameTaken,
    /// Every name candidate is taken.
    #[error("no free name was found")]
    NoFreeName,
    /// An I/O error.
    #[error("{message}")]
    Io {
        /// Kind of the error.
        kind: io::ErrorKind,
        /// Description for the user.
        message: String,
    },
}

/// Fraction of a job that is done, in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Fraction(f64);

impl Fraction {
    /// Returns `None` unless `value` is in `0.0..=1.0`.
    pub fn new(value: f64) -> Option<Self> {
        (0.0..=1.0).contains(&value).then_some(Self(value))
    }

    /// The fraction as a number in `0.0..=1.0`.
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// Progress of a job.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Progress {
    /// Running, amount done unknown.
    Indeterminate,
    /// This much is done.
    Fraction(Fraction),
}

/// A message for the user. Adapters turn it into text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The gesture was refused because its target could not be read.
    Refused(ResolveError),
    /// The target has no file name, such as `/`, so no action applies.
    NoFileName,
    /// A job finished.
    JobSucceeded {
        /// The job.
        job: JobId,
        /// What it did.
        operation: OperationSpec,
        /// What it produced.
        output: JobOutput,
    },
    /// A job failed.
    JobFailed {
        /// The job.
        job: JobId,
        /// What it tried to do.
        operation: OperationSpec,
        /// Why it failed.
        error: JobError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_bounds() {
        assert_eq!(Fraction::new(0.0).map(Fraction::get), Some(0.0));
        assert_eq!(Fraction::new(1.0).map(Fraction::get), Some(1.0));
        assert!(Fraction::new(-0.01).is_none());
        assert!(Fraction::new(1.01).is_none());
        assert!(Fraction::new(f64::NAN).is_none());
    }

    #[test]
    fn job_ids_advance_and_wrap() {
        assert_eq!(JobId::FIRST.next().get(), 2);
        assert_eq!(JobId(u64::MAX).next(), JobId(0));
    }
}
