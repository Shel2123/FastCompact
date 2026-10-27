//! The hotkey and the gesture events.
//!
//! The event tap is the only source of gesture events: its signals are
//! converted to Cocoa coordinates on the main thread and sent to the
//! controller. Carbon registers Ctrl+Option+Space only so that the keystroke
//! does not reach the active application, and to notice presses the tap did
//! not see: the tap handles a key-down before Carbon reports it (spike F10),
//! so a Carbon press without a matching tap key-down is refused with a
//! reason, such as secure keyboard entry hiding the keyboard from the tap.
//! Every Carbon press thus gives exactly one of `Invoked`, from the tap, or
//! `InvokeRefused`.

use std::sync::Arc;

use dispatch2::DispatchQueue;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use objc2::MainThreadMarker;
use orbit_app::messages::{AppEvent, ResolveError};
use orbit_app::ports::EventSink;
use thiserror::Error;
use tracing::{debug, error, warn};

use super::coordinates::Displays;
use super::event_tap::log::InputLog;
use super::event_tap::tap::{EventTap, TapStatus};
use super::event_tap::tracker::Signal;
use super::finder_target::unready;

/// Why the hotkey could not be registered.
#[derive(Debug, Error)]
pub enum HotkeyError {
    /// The hotkey manager could not be created.
    #[error("hotkey manager not created: {0}")]
    Manager(global_hotkey::Error),
    /// Ctrl+Option+Space is taken or could not be registered.
    #[error("Ctrl+Option+Space not registered: {0}")]
    Register(global_hotkey::Error),
}

/// Handler for [`EventTap::start`]: sends each tap signal to the controller
/// through `sink`, converted on the main thread.
pub fn forward_signals(sink: Arc<dyn EventSink>) -> impl Fn(Signal) + Send + Sync + 'static {
    move |signal| {
        let sink = Arc::clone(&sink);
        DispatchQueue::main().exec_async(move || {
            let Some(mtm) = MainThreadMarker::new() else {
                error!(?signal, "tap signal delivered off the main thread");
                return;
            };
            let displays = Displays::current(mtm).unwrap_or_else(|| {
                warn!("no displays; tap points stay unconverted");
                Displays {
                    primary_height: 0.0,
                    visible: Vec::new(),
                }
            });
            sink.send(to_event(signal, &displays));
        });
    }
}

fn to_event(signal: Signal, displays: &Displays) -> AppEvent {
    match signal {
        Signal::Invoke { gesture, point } => AppEvent::Invoked {
            gesture,
            point: displays.to_cocoa(point),
            screens: displays.visible.clone(),
        },
        Signal::Move { gesture, point } => AppEvent::PointerMoved {
            gesture,
            point: displays.to_cocoa(point),
        },
        Signal::Release { gesture, point } => AppEvent::Released {
            gesture,
            point: displays.to_cocoa(point),
        },
        Signal::Cancel { gesture } => AppEvent::GestureCancelled { gesture },
    }
}

/// Why a Carbon press the tap did not see cannot start a gesture.
pub fn refusal(status: &TapStatus) -> ResolveError {
    unready(status)
        .unwrap_or_else(|| ResolveError::Failed("hotkey was not seen by the event tap".into()))
}

/// The Carbon registration of Ctrl+Option+Space. Keep it alive while the
/// application runs.
pub struct HotkeyAdapter {
    _manager: GlobalHotKeyManager,
}

impl std::fmt::Debug for HotkeyAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotkeyAdapter").finish_non_exhaustive()
    }
}

impl HotkeyAdapter {
    /// Registers the hotkey. Each Carbon press is matched with a key-down
    /// in `log`; an unmatched press sends `InvokeRefused` to `sink` and asks
    /// `tap` to run again.
    pub fn install(
        _mtm: MainThreadMarker,
        log: Arc<InputLog>,
        tap: EventTap,
        sink: Arc<dyn EventSink>,
    ) -> Result<Self, HotkeyError> {
        let manager = GlobalHotKeyManager::new().map_err(HotkeyError::Manager)?;
        let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
        manager.register(hotkey).map_err(HotkeyError::Register)?;
        let id = hotkey.id();
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.id != id || event.state != HotKeyState::Pressed {
                return;
            }
            if log.match_hotkey() {
                debug!("hotkey press matched a tap key-down");
                return;
            }
            let status = tap.status();
            let reason = refusal(&status);
            warn!(?status, %reason, "hotkey press not seen by the event tap");
            tap.ensure_running();
            sink.send(AppEvent::InvokeRefused { reason });
        }));
        Ok(Self { _manager: manager })
    }
}

#[cfg(test)]
mod tests {
    use objc2_core_graphics::CGEventType;
    use orbit_app::messages::GestureId;
    use orbit_domain::geometry::{ScreenPoint, ScreenRect};

    use super::*;
    use crate::platform::event_tap::tap::{ListedTap, REQUIRED_MASK};
    use crate::platform::event_tap::tracker::QuartzPoint;

    const READY: TapStatus = TapStatus {
        running: true,
        listed: Some(ListedTap {
            enabled: true,
            events_of_interest: REQUIRED_MASK,
        }),
        accessibility: true,
        secure_input: false,
    };

    #[test]
    fn unseen_press_is_refused_with_the_first_cause() {
        let cases = [
            (
                TapStatus {
                    secure_input: true,
                    ..READY
                },
                ResolveError::SecureInputOn,
            ),
            (
                TapStatus {
                    secure_input: true,
                    accessibility: false,
                    running: false,
                    ..READY
                },
                ResolveError::SecureInputOn,
            ),
            (
                TapStatus {
                    accessibility: false,
                    ..READY
                },
                ResolveError::AccessibilityDenied,
            ),
            (
                TapStatus {
                    listed: Some(ListedTap {
                        enabled: true,
                        events_of_interest: REQUIRED_MASK & !(1 << CGEventType::KeyDown.0),
                    }),
                    ..READY
                },
                ResolveError::Failed("event tap is not receiving keyboard events".into()),
            ),
            (
                TapStatus {
                    running: false,
                    ..READY
                },
                ResolveError::Failed("event tap is not receiving keyboard events".into()),
            ),
            (
                READY,
                ResolveError::Failed("hotkey was not seen by the event tap".into()),
            ),
        ];
        for (status, expected) in cases {
            assert_eq!(refusal(&status), expected, "{status:?}");
        }
    }

    #[test]
    fn signals_become_events_in_cocoa_coordinates() {
        let displays = Displays {
            primary_height: 900.0,
            visible: vec![ScreenRect::new(0.0, 0.0, 1440.0, 875.0)],
        };
        let gesture = GestureId(7);
        let at = QuartzPoint::new(700.0, 500.0);
        let point = ScreenPoint::new(700.0, 400.0);
        assert_eq!(
            to_event(Signal::Invoke { gesture, point: at }, &displays),
            AppEvent::Invoked {
                gesture,
                point,
                screens: displays.visible.clone(),
            }
        );
        assert_eq!(
            to_event(Signal::Move { gesture, point: at }, &displays),
            AppEvent::PointerMoved { gesture, point }
        );
        assert_eq!(
            to_event(Signal::Release { gesture, point: at }, &displays),
            AppEvent::Released { gesture, point }
        );
        assert_eq!(
            to_event(Signal::Cancel { gesture }, &displays),
            AppEvent::GestureCancelled { gesture }
        );
    }
}
