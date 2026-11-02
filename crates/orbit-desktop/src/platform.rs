//! The macOS platform: assembles the system adapters for the runtime.

use std::sync::Arc;
use std::thread;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use orbit_app::messages::AppEvent;
use orbit_app::ports::{EventSink, OperationExecutor};
use orbit_domain::operation::FileNamePolicy;
use orbit_macos::main_thread::MainThreadSender;
use orbit_macos::platform::event_tap::log::InputLog;
use orbit_macos::platform::event_tap::tap::{EventTap, TapOptions};
use orbit_macos::platform::finder_target::FinderSelectionResolver;
use orbit_macos::platform::hotkey::{HotkeyAdapter, forward_signals};
use orbit_macos::platform::permissions::{PermissionPrompt, accessibility_trusted};
use orbit_macos::ui::status_item::status_item;
use orbit_runtime::{Handler, Platform, PlatformError, ResolverFactory, Views};
use thiserror::Error;
use tracing::info;

use crate::debug::{self, DebugSettings};
use crate::logging::{LoggingFeedback, LoggingMenuView, LoggingParametersView, LoggingShell};
use crate::stubs::UnavailableExecutor;

/// Why the platform could not be created.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct PlatformStartError(String);

/// What the tap, the sender and the hotkey share on macOS.
pub struct MacPlatform {
    mtm: MainThreadMarker,
    sender: MainThreadSender,
    log: Arc<InputLog>,
    tap: EventTap,
    debug: DebugSettings,
}

impl MacPlatform {
    /// Starts the event tap. Must be called on the main thread. Asks for
    /// Accessibility on the first start; the tap waits for it.
    pub fn new(debug: DebugSettings) -> Result<Self, PlatformStartError> {
        let fail = |err: &dyn std::fmt::Display| PlatformStartError(err.to_string());
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| PlatformStartError("not on the main thread".into()))?;
        info!(
            accessibility = accessibility_trusted(PermissionPrompt::Ask),
            "accessibility checked"
        );
        let log = Arc::new(InputLog::new());
        let sender = MainThreadSender::new(mtm).map_err(|err| fail(&err))?;
        let sink: Arc<dyn EventSink> = Arc::new(sender);
        let options = TapOptions {
            watchdog: !debug.no_poll,
        };
        let tap = EventTap::start(Arc::clone(&log), options, forward_signals(sink))
            .map_err(|err| fail(&err))?;
        Ok(Self {
            mtm,
            sender,
            log,
            tap,
            debug,
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
        let delay = self.debug.resolve_delay;
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
        Views {
            menu: Box::new(LoggingMenuView),
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
            debug,
        } = self;
        let sink: Arc<dyn EventSink> = Arc::new(sender);
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let _status_item = status_item(mtm);
        sender.set_handler(mtm, move |event| {
            if matches!(event, AppEvent::Invoked { .. }) {
                if debug.secure_pulse {
                    debug::pulse_secure_input();
                }
                if !debug.main_delay.is_zero() {
                    thread::sleep(debug.main_delay);
                }
            }
            handler(event);
        })?;
        let _hotkey = HotkeyAdapter::install(mtm, log, tap, sink)?;
        info!(pid = std::process::id(), "runtime started");
        app.run();
        Ok(())
    }
}
