//! The target of a gesture: the one object selected in Finder.
//!
//! [`FinderSelectionResolver`] reads the selection of the focused Finder
//! file area once per gesture (I2-default), then runs a fence through the
//! event tap and judges the read by the input the tap saw between the press
//! and the end of the read. Any input that may have changed the selection
//! refuses the gesture.

mod area;
mod ax;
mod target;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2_app_kit::NSWorkspace;
use orbit_app::messages::ResolveError;
use orbit_app::ports::{ResolveRequest, TargetResolver};
use orbit_domain::target::FileTarget;
use tracing::{debug, info};

use self::area::ReadError;
use self::ax::AxElement;
use super::event_tap::fence::post_marker;
use super::event_tap::log::{FenceOutcome, Guard, InputLog};
use super::event_tap::tap::{EventTap, TapStatus};
use super::permissions::secure_input_enabled;

/// Bundle identifier of Finder.
const FINDER: &str = "com.apple.finder";

/// Longest wait for the tap to see the fence marker.
const FENCE_TIMEOUT: Duration = Duration::from_millis(500);

/// Reads the selected Finder object; runs on the resolver thread.
#[derive(Debug)]
pub struct FinderSelectionResolver {
    log: Arc<InputLog>,
    tap: EventTap,
    delay: Duration,
}

impl FinderSelectionResolver {
    /// A resolver that guards its reads with `log` and the fences of `tap`.
    pub const fn new(log: Arc<InputLog>, tap: EventTap) -> Self {
        Self {
            log,
            tap,
            delay: Duration::ZERO,
        }
    }

    /// Waits `delay` before each read, to test input during a slow read.
    #[must_use]
    pub const fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn read(&self) -> Result<PathBuf, ResolveError> {
        let pid = frontmost_finder()?;
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        area::selected_path(&AxElement::application(pid)).map_err(|err| match err {
            ReadError::Denied => ResolveError::AccessibilityDenied,
            ReadError::Outside(reason) => {
                debug!(%reason, "focus outside the file area");
                ResolveError::OutsideFileArea
            }
            ReadError::Nothing => ResolveError::NothingSelected,
            ReadError::Multiple(count) => {
                debug!(count, "several objects selected");
                ResolveError::MultipleSelected
            }
            ReadError::Failed(reason) => ResolveError::Failed(reason),
        })
    }

    /// Posts a marker after the read and judges the press by the input the
    /// tap saw before the marker.
    fn fence(&self, request: &ResolveRequest) -> Result<Guard, ResolveError> {
        let ticket = self
            .log
            .register_fence()
            .map_err(|err| ResolveError::Failed(err.to_string()))?;
        post_marker(ticket.user_data()).map_err(|err| ResolveError::Failed(err.to_string()))?;
        match ticket.wait(FENCE_TIMEOUT) {
            FenceOutcome::Seen(snapshot) => Ok(self.log.check(request.gesture, &snapshot)),
            FenceOutcome::TimedOut => {
                Err(ResolveError::Failed("input check did not finish".into()))
            }
        }
    }
}

impl TargetResolver for FinderSelectionResolver {
    fn resolve(&mut self, request: &ResolveRequest) -> Result<FileTarget, ResolveError> {
        let started = Instant::now();
        unready(&self.tap.status()).map_or(Ok(()), Err)?;
        let read = self.read();
        let guard = self.fence(request)?;
        // Secure keyboard entry may have hidden input during the read.
        if secure_input_enabled() {
            return Err(ResolveError::SecureInputOn);
        }
        let result = judge(read, guard).and_then(|path| target::file_target(&path));
        info!(
            gesture = request.gesture.0,
            ?guard,
            elapsed = ?started.elapsed(),
            kind = ?result.as_ref().map(|target| target.facts.kind).ok(),
            "target resolved"
        );
        result
    }
}

/// Why the tap cannot guard a read, or `None` if it can.
pub fn unready(status: &TapStatus) -> Option<ResolveError> {
    if status.secure_input {
        Some(ResolveError::SecureInputOn)
    } else if !status.accessibility {
        Some(ResolveError::AccessibilityDenied)
    } else if !status.ready() {
        Some(ResolveError::Failed(
            "event tap is not receiving keyboard events".into(),
        ))
    } else {
        None
    }
}

/// The pid of Finder if it is the active application.
fn frontmost_finder() -> Result<i32, ResolveError> {
    let app = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .ok_or(ResolveError::FileManagerNotActive)?;
    let is_finder = app
        .bundleIdentifier()
        .is_some_and(|id| id.to_string() == FINDER);
    if is_finder {
        Ok(app.processIdentifier())
    } else {
        Err(ResolveError::FileManagerNotActive)
    }
}

/// The read stands only if no input came between the press and the fence.
/// Missing access is reported as is, whatever the input.
fn judge(read: Result<PathBuf, ResolveError>, guard: Guard) -> Result<PathBuf, ResolveError> {
    match (read, guard) {
        (
            Err(
                err @ (ResolveError::AccessibilityDenied
                | ResolveError::InputMonitoringDenied
                | ResolveError::SecureInputOn),
            ),
            _,
        ) => Err(err),
        (_, Guard::Inputs | Guard::ButtonHeld | Guard::Interrupted) => {
            Err(ResolveError::SelectionUnstable)
        }
        (_, Guard::UnknownPress) => Err(ResolveError::Failed(
            "the press was not seen by the event tap".into(),
        )),
        (read, Guard::Clean) => read,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::event_tap::tap::{ListedTap, REQUIRED_MASK};

    #[test]
    fn judge_prefers_access_then_input_then_the_read() {
        let path = || Ok(PathBuf::from("/tmp/a"));
        assert_eq!(judge(path(), Guard::Clean), path());
        for guard in [Guard::Inputs, Guard::ButtonHeld, Guard::Interrupted] {
            assert_eq!(judge(path(), guard), Err(ResolveError::SelectionUnstable));
            assert_eq!(
                judge(Err(ResolveError::NothingSelected), guard),
                Err(ResolveError::SelectionUnstable)
            );
            assert_eq!(
                judge(Err(ResolveError::AccessibilityDenied), guard),
                Err(ResolveError::AccessibilityDenied)
            );
            assert_eq!(
                judge(Err(ResolveError::SecureInputOn), guard),
                Err(ResolveError::SecureInputOn)
            );
        }
        assert!(matches!(
            judge(path(), Guard::UnknownPress),
            Err(ResolveError::Failed(_))
        ));
        for err in [
            ResolveError::OutsideFileArea,
            ResolveError::NothingSelected,
            ResolveError::MultipleSelected,
            ResolveError::FileManagerNotActive,
        ] {
            assert_eq!(judge(Err(err.clone()), Guard::Clean), Err(err));
        }
    }

    #[test]
    fn unready_names_the_first_missing_condition() {
        let ready = TapStatus {
            running: true,
            listed: Some(ListedTap {
                enabled: true,
                events_of_interest: REQUIRED_MASK,
            }),
            accessibility: true,
            secure_input: false,
        };
        assert_eq!(unready(&ready), None);
        assert_eq!(
            unready(&TapStatus {
                secure_input: true,
                accessibility: false,
                ..ready
            }),
            Some(ResolveError::SecureInputOn)
        );
        assert_eq!(
            unready(&TapStatus {
                accessibility: false,
                running: false,
                ..ready
            }),
            Some(ResolveError::AccessibilityDenied)
        );
        assert!(matches!(
            unready(&TapStatus {
                listed: None,
                ..ready
            }),
            Some(ResolveError::Failed(_))
        ));
    }
}
