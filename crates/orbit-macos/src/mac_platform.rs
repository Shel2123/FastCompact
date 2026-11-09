//! The macOS platform: assembles the system adapters for the runtime.

use std::sync::Arc;
use std::thread;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use orbit_app::messages::AppEvent;
use orbit_app::ports::{EventSink, MenuView, OperationExecutor};
use orbit_domain::operation::FileNamePolicy;
use orbit_runtime::{Handler, Platform, PlatformError, ResolverFactory, Views};
use thiserror::Error;
use tracing::{error, info};

use crate::debug::{self, MacOptions};
use crate::main_thread::{MainThreadError, MainThreadSender};
use crate::operations::unavailable::UnavailableExecutor;
use crate::platform::event_tap::log::InputLog;
use crate::platform::event_tap::tap::{EventTap, TapError, TapOptions};
use crate::platform::finder_target::FinderSelectionResolver;
use crate::platform::hotkey::{HotkeyAdapter, HotkeyError, forward_signals};
use crate::platform::permissions::{PermissionPrompt, accessibility_trusted};
use crate::ui::logging::{LoggingFeedback, LoggingMenuView, LoggingParametersView, LoggingShell};
use crate::ui::radial_panel::RadialMenu;
use crate::ui::status_item::status_item;

/// Why the platform could not be created or started.
#[derive(Debug, Error)]
pub enum MacPlatformError {
    /// The platform must be created on the main thread.
    #[error("the macOS platform was not created on the main thread")]
    NotMainThread,
    /// The delivery of events to the main thread could not be set up.
    #[error(transparent)]
    MainThread(#[from] MainThreadError),
    /// The event tap could not be started.
    #[error(transparent)]
    Tap(#[from] TapError),
    /// The hotkey could not be registered.
    #[error(transparent)]
    Hotkey(#[from] HotkeyError),
}

/// The adapters that the tap, the sender and the hotkey share.
///
/// The event tap starts in [`new`](Self::new), before the runtime exists,
/// because the resolver reads it. Its signals wait in the sink until the
/// handler is set. The hotkey is registered in [`run`](Platform::run), after
/// the handler.
pub struct MacPlatform {
    mtm: MainThreadMarker,
    sender: MainThreadSender,
    log: Arc<InputLog>,
    tap: EventTap,
    options: MacOptions,
}

impl MacPlatform {
    /// Starts the event tap. Asks for Accessibility on the first start; the
    /// tap waits for it.
    ///
    /// # Errors
    ///
    /// [`MacPlatformError`] when not called on the main thread, or when the
    /// sender or the tap cannot be created.
    pub fn new(options: MacOptions) -> Result<Self, MacPlatformError> {
        let mtm = MainThreadMarker::new().ok_or(MacPlatformError::NotMainThread)?;
        // Asks for Accessibility on the first start; the tap waits for it.
        info!(
            accessibility = accessibility_trusted(PermissionPrompt::Ask),
            "accessibility checked"
        );
        let log = Arc::new(InputLog::new());
        let sender = MainThreadSender::new(mtm)?;
        let sink: Arc<dyn EventSink> = Arc::new(sender);
        let tap_options = TapOptions {
            watchdog: !options.no_poll,
        };
        let tap = EventTap::start(Arc::clone(&log), tap_options, forward_signals(sink))?;
        Ok(Self {
            mtm,
            sender,
            log,
            tap,
            options,
        })
    }
}

impl Platform for MacPlatform {
    fn sink(&self) -> Arc<dyn EventSink> {
        Arc::new(self.sender)
    }

    fn resolver(&self) -> ResolverFactory {
        let log = Arc::clone(&self.log);
        let tap = self.tap.clone();
        let delay = self.options.resolve_delay;
        ResolverFactory::new(move || FinderSelectionResolver::new(log, tap).with_delay(delay))
    }

    fn executor(&self) -> Arc<dyn OperationExecutor> {
        Arc::new(UnavailableExecutor)
    }

    fn file_name_policy(&self) -> FileNamePolicy {
        // Finder shows ':' as '/'.
        FileNamePolicy::UNIVERSAL.forbidding(&[':'])
    }

    fn views(&mut self) -> Views {
        let menu: Box<dyn MenuView> = match RadialMenu::new(
            self.mtm,
            self.options.menu_background,
            self.options.menu_shift,
        ) {
            Ok(menu) => Box::new(menu),
            Err(err) => {
                // The gesture still works, but nothing is drawn: the user
                // would release the key without seeing the menu.
                error!(%err, "the radial menu cannot be built; the menu is only logged");
                Box::new(LoggingMenuView)
            }
        };
        Views {
            menu,
            parameters: Box::new(LoggingParametersView),
            feedback: Box::new(LoggingFeedback),
            shell: Box::new(LoggingShell),
        }
    }

    fn run(self, mut handler: Handler) -> Result<(), PlatformError> {
        let Self {
            mtm,
            sender,
            log,
            tap,
            options,
        } = self;
        let sink: Arc<dyn EventSink> = Arc::new(sender);
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let _status_item = status_item(mtm);
        run_in_order(
            || {
                sender
                    .set_handler(mtm, move |event| {
                        if matches!(event, AppEvent::Invoked { .. }) {
                            if options.secure_pulse {
                                debug::pulse_secure_input();
                            }
                            if !options.main_delay.is_zero() {
                                thread::sleep(options.main_delay);
                            }
                        }
                        handler(event);
                    })
                    .map_err(MacPlatformError::from)
            },
            || HotkeyAdapter::install(mtm, log, tap, sink).map_err(MacPlatformError::from),
            |_hotkey| {
                info!(pid = std::process::id(), "runtime started");
                app.run();
            },
        )
        .map_err(Into::into)
    }
}

/// The steps of [`Platform::run`] in the order the platform guarantees:
/// the handler first, then the hotkey, then the event loop.
///
/// The hotkey adapter can send an event as soon as it is registered, so the
/// handler must exist by then. A step that fails ends the run: later steps
/// do not start. The value of the hotkey step lives until the loop returns.
fn run_in_order<K, E>(
    set_handler: impl FnOnce() -> Result<(), E>,
    install_hotkey: impl FnOnce() -> Result<K, E>,
    event_loop: impl FnOnce(&K),
) -> Result<(), E> {
    set_handler()?;
    let hotkey = install_hotkey()?;
    event_loop(&hotkey);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Runs `run_in_order` with steps that record their name and fail where
    /// `fail` says.
    fn steps(fail: Option<&'static str>) -> (Vec<&'static str>, Result<(), &'static str>) {
        let log = RefCell::new(Vec::new());
        let step = |name: &'static str| {
            log.borrow_mut().push(name);
            if fail == Some(name) {
                Err(name)
            } else {
                Ok(())
            }
        };
        let result = run_in_order(
            || step("set_handler"),
            || step("install_hotkey"),
            |_| {
                log.borrow_mut().push("event_loop");
            },
        );
        (log.into_inner(), result)
    }

    #[test]
    fn handler_then_hotkey_then_loop() {
        let (log, result) = steps(None);
        assert_eq!(log, ["set_handler", "install_hotkey", "event_loop"]);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn failed_handler_stops_before_the_hotkey_and_the_loop() {
        let (log, result) = steps(Some("set_handler"));
        assert_eq!(log, ["set_handler"]);
        assert_eq!(result, Err("set_handler"));
    }

    #[test]
    fn failed_hotkey_stops_before_the_loop() {
        let (log, result) = steps(Some("install_hotkey"));
        assert_eq!(log, ["set_handler", "install_hotkey"]);
        assert_eq!(result, Err("install_hotkey"));
    }
}
