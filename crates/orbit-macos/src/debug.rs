//! Options of the macOS platform, including the switches for the manual
//! checks of stage 4 (`docs/stage-4.md`).
//!
//! Read from the environment in debug builds only; a release build always
//! runs with the defaults.

use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime};
use tracing::{info, warn};

use crate::ui::radial_panel::Background;

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
    /// `ORBIT_DEBUG_MENU_BACKGROUND=glass|vibrancy|plain`: the background of
    /// the menu, to reach the fallbacks on a system that has glass.
    pub menu_background: Option<Background>,
    /// `ORBIT_DEBUG_MENU_SHIFT=dx,dy`: the menu window is put this many
    /// points away from where it should be, to see the disc follow it.
    pub menu_shift: (i32, i32),
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
            menu_background: menu_background(),
            menu_shift: menu_shift(),
        };
        if settings != Self::default() {
            warn!(?settings, "debug switches are on");
        }
        settings
    }
}

/// `ORBIT_DEBUG_MENU_BACKGROUND`, or `None` if it is unset or not a background.
fn menu_background() -> Option<Background> {
    let name = std::env::var("ORBIT_DEBUG_MENU_BACKGROUND").ok()?;
    let background = Background::parse(&name);
    if background.is_none() {
        warn!(
            name,
            "ORBIT_DEBUG_MENU_BACKGROUND is not glass, vibrancy or plain; ignored"
        );
    }
    background
}

/// `ORBIT_DEBUG_MENU_SHIFT`, or `(0, 0)` if it is unset or not `dx,dy`.
fn menu_shift() -> (i32, i32) {
    let Ok(text) = std::env::var("ORBIT_DEBUG_MENU_SHIFT") else {
        return (0, 0);
    };
    parse_shift(&text).unwrap_or_else(|| {
        warn!(
            text,
            "ORBIT_DEBUG_MENU_SHIFT is not dx,dy in whole points; ignored"
        );
        (0, 0)
    })
}

/// `"6,-4"` as `(6, -4)`.
fn parse_shift(text: &str) -> Option<(i32, i32)> {
    let (dx, dy) = text.split_once(',')?;
    Some((dx.trim().parse().ok()?, dy.trim().parse().ok()?))
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

#[cfg(test)]
mod tests {
    use super::parse_shift;

    #[test]
    fn shift_is_two_whole_numbers() {
        assert_eq!(parse_shift("6,-4"), Some((6, -4)));
        assert_eq!(parse_shift(" 14 , 0 "), Some((14, 0)));
        assert_eq!(parse_shift("6"), None);
        assert_eq!(parse_shift("6,"), None);
        assert_eq!(parse_shift("a,b"), None);
        assert_eq!(parse_shift("1.5,2"), None);
        assert_eq!(parse_shift("1,2,3"), None);
    }
}
