//! Options of the macOS platform, including the switches for the manual
//! checks of stage 4 (`docs/stage-4.md`).
//!
//! Read from the environment in debug builds only; a release build always
//! runs with the defaults.

use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime};
use tracing::{info, warn};

/// When the secure input pulse starts and ends, after the press.
const PULSE_ON: Duration = Duration::from_millis(200);
const PULSE_OFF: Duration = Duration::from_millis(700);

/// Options of [`MacPlatform`](crate::MacPlatform); all off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MacOptions {
    /// `ORBIT_DEBUG_RESOLVE_DELAY_MS`: the resolver waits this long before
    /// each read.
    pub resolve_delay: Duration,
    /// `ORBIT_DEBUG_MAIN_DELAY_MS`: the main thread sleeps this long before
    /// it handles a press.
    pub main_delay: Duration,
    /// `ORBIT_DEBUG_SECURE_PULSE=1`: secure keyboard entry turns on 200 ms
    /// after each press and off 500 ms later.
    pub secure_pulse: bool,
    /// `ORBIT_DEBUG_NO_POLL=1`: the tap runs without its watchdog.
    pub no_poll: bool,
}

impl MacOptions {
    /// The switches set in the environment of a debug build. A release build
    /// always returns the defaults.
    pub fn from_env() -> Self {
        if !cfg!(debug_assertions) {
            return Self::default();
        }
        let millis = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .map_or(Duration::ZERO, Duration::from_millis)
        };
        let flag = |name: &str| std::env::var(name).is_ok_and(|value| value == "1");
        let settings = Self {
            resolve_delay: millis("ORBIT_DEBUG_RESOLVE_DELAY_MS"),
            main_delay: millis("ORBIT_DEBUG_MAIN_DELAY_MS"),
            secure_pulse: flag("ORBIT_DEBUG_SECURE_PULSE"),
            no_poll: flag("ORBIT_DEBUG_NO_POLL"),
        };
        if settings != Self::default() {
            warn!(?settings, "debug switches are on");
        }
        settings
    }
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn EnableSecureEventInput() -> i32;
    fn DisableSecureEventInput() -> i32;
}

/// Turns secure keyboard entry on and off again shortly after a press, so
/// that a release can happen while the tap sees no keys.
pub fn pulse_secure_input() {
    let main = DispatchQueue::main();
    let at = |delay| DispatchTime::try_from(delay).unwrap_or(DispatchTime::NOW);
    let on = main.after(at(PULSE_ON), || {
        // SAFETY: takes no arguments; balanced by the call below.
        let status = unsafe { EnableSecureEventInput() };
        info!(status, "debug: secure input on");
    });
    let off = main.after(at(PULSE_OFF), || {
        // SAFETY: takes no arguments; follows the enable above, which runs
        // first on the serial main queue.
        let status = unsafe { DisableSecureEventInput() };
        info!(status, "debug: secure input off");
    });
    if on.is_err() || off.is_err() {
        warn!("debug: secure input pulse not scheduled");
    }
}
