//! Application state.
//!
//! The menu and the jobs live independently: closing the menu never cancels
//! a job.

use std::collections::HashMap;

use orbit_domain::geometry::ScreenPoint;
use orbit_domain::target::FileTarget;

use crate::catalog::{SLOT_COUNT, SlotModel};
use crate::messages::{GestureId, JobId, JobSpec, ParametersKind, Progress};

/// Everything the controller keeps.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppState {
    /// The hotkey press that is held right now. Set by
    /// [`Invoked`](crate::messages::AppEvent::Invoked) and cleared only by
    /// the [`Released`](crate::messages::AppEvent::Released) of the same
    /// press, so a refusal or Esc does not let a repeated press reopen the
    /// menu.
    pub gesture: Option<GestureId>,
    /// What the menu shows.
    pub menu: MenuState,
    /// Jobs that were started and have not finished.
    pub jobs: HashMap<JobId, JobState>,
}

/// Phase of the menu.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum MenuState {
    /// Nothing is shown.
    #[default]
    Closed,
    /// The target of a gesture is being read; nothing is shown yet.
    Resolving {
        /// The press.
        gesture: GestureId,
        /// Where the hotkey was pressed.
        invoke: ScreenPoint,
        /// Where the menu will be centered.
        center: ScreenPoint,
        /// Last cursor position seen while resolving.
        cursor: Option<ScreenPoint>,
    },
    /// The radial menu is shown.
    Open {
        /// The press.
        gesture: GestureId,
        /// Where the hotkey was pressed.
        invoke: ScreenPoint,
        /// Menu center.
        center: ScreenPoint,
        /// The target, fixed for the rest of the gesture.
        target: FileTarget,
        /// The slots with their availability for `target`.
        slots: [SlotModel; SLOT_COUNT],
        /// What is highlighted.
        highlight: Highlight,
    },
    /// The parameters form is shown.
    Parameters {
        /// The press that opened the form.
        gesture: GestureId,
        /// The target.
        target: FileTarget,
        /// What the form asks for.
        kind: ParametersKind,
    },
}

/// What the open menu highlights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Highlight {
    /// No slot: the cursor is in the neutral zone, has not moved, or is
    /// outside the menu.
    #[default]
    None,
    /// A slot that runs when the key is released.
    Active(usize),
    /// An unavailable action, shown dimmed; releasing over it does nothing.
    Inactive(usize),
}

/// A job that was started and has not finished.
#[derive(Debug, Clone, PartialEq)]
pub struct JobState {
    /// What the job does.
    pub spec: JobSpec,
    /// Last reported progress.
    pub progress: Progress,
    /// Whether cancellation was requested.
    pub cancelling: bool,
}
