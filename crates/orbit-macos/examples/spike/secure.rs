//! Secure Event Input pulse for stage 4 step 0c (F7b): turns Secure Input on
//! shortly after the hotkey press and off as soon as Carbon reports the
//! Space release, so that it is on only while Space goes up — shorter than
//! the watchdog period would need to notice.
//!
//! The pulse comes either from the spike itself or from a helper process
//! (the same executable started with [`HELPER_ARG`]), in case Secure Input
//! of the spike's own process does not hide the keyboard from its own tap.

use std::io::{BufRead, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use tracing::{error, info, warn};

use crate::tap;

/// Argument that turns the executable into the pulse helper.
pub const HELPER_ARG: &str = "--secure-helper";

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn EnableSecureEventInput() -> i32;
    fn DisableSecureEventInput() -> i32;
}

fn enable() -> i32 {
    // SAFETY: takes no arguments; balanced by `disable`.
    unsafe { EnableSecureEventInput() }
}

fn disable() -> i32 {
    // SAFETY: takes no arguments; only called after a matching `enable`.
    unsafe { DisableSecureEventInput() }
}

/// Which process turns Secure Input on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    OwnProcess,
    Helper,
}

/// Secure Input pulse state, on the main thread.
pub struct Pulse {
    source: Source,
    helper: Option<(Child, ChildStdin)>,
    on: bool,
    /// Number of the last armed pulse; a scheduled enable or fallback for
    /// an older one is ignored.
    armed: u64,
}

impl Pulse {
    pub fn new(source: Source) -> Self {
        let helper = match source {
            Source::OwnProcess => None,
            Source::Helper => spawn_helper(),
        };
        Self {
            source,
            helper,
            on: false,
            armed: 0,
        }
    }

    /// Starts a new pulse and returns its number.
    pub fn arm(&mut self) -> u64 {
        self.armed += 1;
        self.armed
    }

    /// Turns Secure Input on for the pulse `armed`, unless a newer pulse
    /// was armed or it is already on.
    pub fn enable(&mut self, armed: u64) {
        if armed != self.armed || self.on {
            return;
        }
        let status = match self.source {
            Source::OwnProcess => enable(),
            Source::Helper => self.send("on"),
        };
        self.on = true;
        info!(
            pulse = armed,
            source = ?self.source,
            status,
            secure_input = tap::secure_input_enabled(),
            "secure input pulse on"
        );
    }

    /// Turns Secure Input off if this pulse turned it on.
    pub fn disable(&mut self, reason: &str) {
        if !self.on {
            return;
        }
        let status = match self.source {
            Source::OwnProcess => disable(),
            Source::Helper => self.send("off"),
        };
        self.on = false;
        info!(
            pulse = self.armed,
            reason,
            status,
            secure_input = tap::secure_input_enabled(),
            "secure input pulse off"
        );
    }

    /// Turns Secure Input off if pulse `armed` is still the current one.
    pub fn expire(&mut self, armed: u64) {
        if armed == self.armed {
            self.disable("fallback timeout");
        }
    }

    /// Sends a command to the helper. Returns 0 on success, -1 otherwise,
    /// like an `OSStatus`.
    fn send(&mut self, command: &str) -> i32 {
        let Some((_, stdin)) = self.helper.as_mut() else {
            warn!(command, "secure input helper is not running");
            return -1;
        };
        match writeln!(stdin, "{command}").and_then(|()| stdin.flush()) {
            Ok(()) => 0,
            Err(err) => {
                warn!(command, %err, "secure input helper did not take the command");
                -1
            }
        }
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        self.disable("spike stopping");
        if let Some((mut child, stdin)) = self.helper.take() {
            // Closing stdin ends the helper.
            drop(stdin);
            if let Err(err) = child.wait() {
                warn!(%err, "secure input helper not reaped");
            }
        }
    }
}

fn spawn_helper() -> Option<(Child, ChildStdin)> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            error!(%err, "own executable unknown; secure input helper not started");
            return None;
        }
    };
    let spawned = Command::new(exe)
        .arg(HELPER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            let Some(stdin) = child.stdin.take() else {
                error!("secure input helper has no stdin");
                return None;
            };
            info!(pid = child.id(), "secure input helper started");
            Some((child, stdin))
        }
        Err(err) => {
            error!(%err, "secure input helper not started");
            None
        }
    }
}

/// Body of the helper process: `on` and `off` lines from stdin turn Secure
/// Input on and off; end of input turns it off and exits.
pub fn helper_main() {
    let mut on = false;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else {
            break;
        };
        match line.trim() {
            "on" if !on => {
                enable();
                on = true;
            }
            "off" if on => {
                disable();
                on = false;
            }
            _ => {}
        }
    }
    if on {
        disable();
    }
}
