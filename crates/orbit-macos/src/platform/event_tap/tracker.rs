//! Gesture state on the tap thread.
//!
//! [`GestureTracker`] sees every event of the tap in order and decides, in
//! the same call, whether the event goes on to the applications and which
//! gesture signals it produces. Reading HID counters and posting markers go
//! through [`TapIo`], so the tracker runs in tests without a tap.
//!
//! # Continuity check
//!
//! Under secure keyboard entry the tap does not see the keyboard, and a
//! release of the hotkey can pass unseen. A release seen by the tap is
//! therefore only trusted when the tap saw every key event of the hold:
//!
//! 1. At the press the tracker reads the HID key counter `B` and posts a
//!    baseline marker; when the marker comes back, `S0` is the number of key
//!    events the tap has seen.
//! 2. At the release it reads the counter `C` and posts a release marker;
//!    when that marker comes back, `S1` is the number of key events seen.
//! 3. The release stands only if `C - B == S1 - S0`. Any difference, a
//!    marker that fails to post or does not come back within
//!    [`RELEASE_TIMEOUT`] cancels the gesture.
//!
//! The counter is read before the marker is posted, and events reach the tap
//! in the order the system queued them, so a key event counted in `B` or `C`
//! reaches the tap before the matching marker. HID counters skip autorepeat,
//! so the tap skips it too.

use std::sync::Arc;
use std::time::{Duration, Instant};

use orbit_app::messages::GestureId;
use tracing::{debug, warn};

use super::log::{InputLog, Press};
use super::{marker_nonce, marker_user_data};

/// Virtual key code of Space.
pub const SPACE_KEYCODE: u16 = 0x31;

/// Virtual key code of Escape.
pub const ESCAPE_KEYCODE: u16 = 0x35;

/// How long the release marker may take before the gesture is cancelled.
pub const RELEASE_TIMEOUT: Duration = Duration::from_millis(100);

/// A position in Quartz coordinates: origin at the top left of the primary
/// display, y growing down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuartzPoint {
    /// Horizontal position.
    pub x: f64,
    /// Vertical position.
    pub y: f64,
}

impl QuartzPoint {
    /// The point at `x`, `y`.
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Modifier keys held at an event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    /// Control.
    pub control: bool,
    /// Option.
    pub option: bool,
    /// Command.
    pub command: bool,
    /// Shift.
    pub shift: bool,
}

impl Modifiers {
    /// Control and Option, nothing else: the modifiers of the hotkey.
    pub const HOTKEY: Self = Self {
        control: true,
        option: true,
        command: false,
        shift: false,
    };

    /// Exactly the hotkey modifiers, as Carbon matches them.
    const fn press_hotkey(self) -> bool {
        self.control && self.option && !self.command && !self.shift
    }

    /// The hotkey stays held while Control and Option are both down.
    const fn hold_hotkey(self) -> bool {
        self.control && self.option
    }
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// The left button.
    Left,
    /// The right button.
    Right,
    /// Any other button.
    Other,
}

impl Button {
    const fn bit(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Other => 4,
        }
    }
}

/// An event seen by the tap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TapEvent {
    /// A key went down, or repeats while held.
    KeyDown {
        /// Virtual key code.
        keycode: u16,
        /// The key repeats while held.
        autorepeat: bool,
        /// Modifiers held.
        modifiers: Modifiers,
        /// Cursor position.
        point: QuartzPoint,
    },
    /// A key went up.
    KeyUp {
        /// Virtual key code.
        keycode: u16,
        /// Cursor position.
        point: QuartzPoint,
    },
    /// A modifier key went down or up.
    FlagsChanged {
        /// Modifiers held after the change.
        modifiers: Modifiers,
        /// Cursor position.
        point: QuartzPoint,
    },
    /// A mouse button went down.
    MouseDown(Button),
    /// A mouse button went up.
    MouseUp(Button),
    /// The cursor moved with no button down.
    MouseMoved(QuartzPoint),
    /// The cursor moved with a button down.
    MouseDragged(QuartzPoint),
    /// A scroll wheel event, which is also how markers travel.
    Scroll {
        /// The `EventSourceUserData` field.
        user_data: i64,
    },
    /// The system disabled the tap, or the tap was removed; events may have
    /// passed unseen.
    TapDisabled,
}

/// HID key counters, which count key events before secure keyboard entry
/// hides them. Autorepeat is not counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HidKeys {
    /// Key-down events.
    pub down: u32,
    /// Key-up events.
    pub up: u32,
}

impl HidKeys {
    /// Key events counted since `earlier`. The counters wrap.
    fn since(self, earlier: Self) -> u64 {
        u64::from(self.down.wrapping_sub(earlier.down))
            + u64::from(self.up.wrapping_sub(earlier.up))
    }
}

/// What the tracker reads from and does to the system.
pub trait TapIo {
    /// Current HID key counters.
    fn keyboard_counter(&mut self) -> HidKeys;
    /// Whether the HID state has a mouse button down.
    fn button_held(&mut self) -> bool;
    /// Posts a marker event with `user_data`. Returns `false` if it could
    /// not be posted.
    fn post_marker(&mut self, user_data: i64) -> bool;
}

/// Input state read on a timer while a gesture is held, to notice what the
/// tap cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollState {
    /// Secure keyboard entry is on.
    pub secure_input: bool,
    /// The HID state has Control down.
    pub control: bool,
    /// The HID state has Option down.
    pub option: bool,
}

/// A gesture signal, sent to the main thread in order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    /// The hotkey went down.
    Invoke {
        /// The new gesture.
        gesture: GestureId,
        /// Cursor position of the key-down.
        point: QuartzPoint,
    },
    /// The cursor moved during the gesture.
    Move {
        /// The gesture.
        gesture: GestureId,
        /// New cursor position.
        point: QuartzPoint,
    },
    /// The gesture ended.
    Release {
        /// The gesture.
        gesture: GestureId,
        /// Cursor position at the release.
        point: QuartzPoint,
    },
    /// The gesture must do nothing: Esc, or the tap lost sight of it.
    Cancel {
        /// The gesture.
        gesture: GestureId,
    },
}

/// Whether an event goes on to the applications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Deliver the event.
    Pass,
    /// Drop the event.
    Swallow,
}

/// Result of one call of the tracker.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// What to do with the event.
    pub disposition: Disposition,
    /// Signals to send, in order.
    pub signals: Vec<Signal>,
}

impl Outcome {
    const fn pass() -> Self {
        Self {
            disposition: Disposition::Pass,
            signals: Vec::new(),
        }
    }
}

/// The baseline of the continuity check.
#[derive(Debug, Clone, Copy)]
struct Baseline {
    /// HID counters at the press.
    hid: HidKeys,
    /// Nonce of the baseline marker.
    nonce: u32,
    /// Key events seen by the tap when the marker came back.
    seen: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
enum Hold {
    #[default]
    Idle,
    /// The hotkey is down. `baseline` is `None` if its marker failed to
    /// post; the gesture is cancelled then and ends at the release.
    Held {
        gesture: GestureId,
        point: QuartzPoint,
        baseline: Option<Baseline>,
    },
    /// The tap saw the release and waits for the release marker.
    Releasing {
        gesture: GestureId,
        point: QuartzPoint,
        baseline: Baseline,
        hid: HidKeys,
        nonce: u32,
        deadline: Instant,
    },
}

/// Gesture state machine of the tap thread.
#[derive(Debug)]
pub struct GestureTracker {
    log: Arc<InputLog>,
    hold: Hold,
    next_gesture: u64,
    /// Key-down (without autorepeat) and key-up events seen, swallowed ones
    /// included.
    keyboard_seen: u64,
    /// Buttons down, by the events the tap saw.
    buttons: u8,
    /// The last Esc key-down was swallowed, so its key-up is too.
    escape_swallowed: bool,
}

impl GestureTracker {
    /// A tracker with no gesture that records into `log`.
    pub fn new(log: Arc<InputLog>) -> Self {
        Self {
            log,
            hold: Hold::Idle,
            next_gesture: 1,
            keyboard_seen: 0,
            buttons: 0,
            escape_swallowed: false,
        }
    }

    /// Whether a gesture is in progress, so [`on_poll`](Self::on_poll) must
    /// run.
    pub const fn needs_poll(&self) -> bool {
        !matches!(self.hold, Hold::Idle)
    }

    /// Handles one event of the tap, seen at `now`.
    pub fn on_event(&mut self, event: TapEvent, now: Instant, io: &mut impl TapIo) -> Outcome {
        let seq = self.log.next_seq();
        let mut out = Outcome::pass();
        self.expire(now, &mut out);
        match event {
            TapEvent::KeyDown {
                keycode,
                autorepeat,
                modifiers,
                point,
            } => {
                if !autorepeat {
                    self.keyboard_seen += 1;
                }
                if keycode == SPACE_KEYCODE && modifiers.press_hotkey() {
                    if !autorepeat {
                        self.press(seq, point, io, &mut out);
                    }
                } else if keycode == ESCAPE_KEYCODE && matches!(self.hold, Hold::Held { .. }) {
                    self.escape_swallowed = true;
                    out.disposition = Disposition::Swallow;
                    if let Hold::Held { gesture, .. } = self.hold
                        && !autorepeat
                    {
                        out.signals.push(Signal::Cancel { gesture });
                    }
                } else {
                    if keycode == ESCAPE_KEYCODE {
                        self.escape_swallowed = false;
                    }
                    self.log.record_input(seq);
                }
            }
            TapEvent::KeyUp { keycode, point } => {
                self.keyboard_seen += 1;
                if keycode == ESCAPE_KEYCODE && self.escape_swallowed {
                    self.escape_swallowed = false;
                    out.disposition = Disposition::Swallow;
                } else if keycode == SPACE_KEYCODE {
                    self.release(point, now, io, &mut out);
                }
            }
            TapEvent::FlagsChanged { modifiers, point } => {
                if !modifiers.hold_hotkey() {
                    self.release(point, now, io, &mut out);
                }
            }
            TapEvent::MouseDown(button) => {
                self.buttons |= button.bit();
                self.log.record_input(seq);
            }
            TapEvent::MouseUp(button) => {
                self.buttons &= !button.bit();
                self.log.record_input(seq);
            }
            TapEvent::MouseMoved(point) => self.moved(point, &mut out),
            TapEvent::MouseDragged(point) => {
                self.log.record_input(seq);
                self.moved(point, &mut out);
            }
            TapEvent::Scroll { user_data } => {
                if let Some(nonce) = marker_nonce(user_data) {
                    out.disposition = Disposition::Swallow;
                    self.marker(nonce, seq, &mut out);
                }
            }
            TapEvent::TapDisabled => {
                self.log.record_interrupt();
                self.end(&mut out);
            }
        }
        out
    }

    /// Checks on a timer what the tap cannot see while a gesture is held.
    pub fn on_poll(&mut self, poll: PollState, now: Instant) -> Outcome {
        let mut out = Outcome::pass();
        self.expire(now, &mut out);
        if let Hold::Held { gesture, .. } = self.hold
            && (poll.secure_input || !poll.control || !poll.option)
        {
            warn!(
                ?gesture,
                ?poll,
                "watch over the gesture lost; cancelling it"
            );
            self.fail(&mut out);
        }
        out
    }

    fn issue(&mut self) -> GestureId {
        let gesture = GestureId(self.next_gesture);
        self.next_gesture = self.next_gesture.wrapping_add(1);
        gesture
    }

    fn press(&mut self, seq: u64, point: QuartzPoint, io: &mut impl TapIo, out: &mut Outcome) {
        if let Hold::Held { gesture, .. } | Hold::Releasing { gesture, .. } = self.hold {
            warn!(?gesture, "new press before the release was confirmed");
            self.end(out);
        }
        let gesture = self.issue();
        let hid = io.keyboard_counter();
        let button_held = self.buttons != 0 || io.button_held();
        self.log.record_press(gesture, Press { seq, button_held });
        out.signals.push(Signal::Invoke { gesture, point });
        let nonce = self.log.next_nonce();
        let baseline = if io.post_marker(marker_user_data(nonce)) {
            Some(Baseline {
                hid,
                nonce,
                seen: None,
            })
        } else {
            warn!(
                ?gesture,
                "baseline marker not posted; cancelling the gesture"
            );
            self.log.record_interrupt();
            out.signals.push(Signal::Cancel { gesture });
            None
        };
        self.hold = Hold::Held {
            gesture,
            point,
            baseline,
        };
    }

    fn release(
        &mut self,
        point: QuartzPoint,
        now: Instant,
        io: &mut impl TapIo,
        out: &mut Outcome,
    ) {
        let Hold::Held {
            gesture, baseline, ..
        } = self.hold
        else {
            return;
        };
        self.hold = Hold::Held {
            gesture,
            point,
            baseline,
        };
        let Some(baseline) = baseline else {
            self.fail(out);
            return;
        };
        let hid = io.keyboard_counter();
        let nonce = self.log.next_nonce();
        if !io.post_marker(marker_user_data(nonce)) {
            warn!(
                ?gesture,
                "release marker not posted; cancelling the gesture"
            );
            self.fail(out);
            return;
        }
        self.hold = Hold::Releasing {
            gesture,
            point,
            baseline,
            hid,
            nonce,
            deadline: now + RELEASE_TIMEOUT,
        };
    }

    fn moved(&mut self, to: QuartzPoint, out: &mut Outcome) {
        if let Hold::Held { gesture, point, .. } = &mut self.hold {
            *point = to;
            out.signals.push(Signal::Move {
                gesture: *gesture,
                point: to,
            });
        }
    }

    fn marker(&mut self, nonce: u32, seq: u64, out: &mut Outcome) {
        let seen = self.keyboard_seen;
        match &mut self.hold {
            Hold::Held {
                baseline: Some(baseline),
                ..
            }
            | Hold::Releasing { baseline, .. }
                if baseline.nonce == nonce && baseline.seen.is_none() =>
            {
                baseline.seen = Some(seen);
            }
            Hold::Releasing {
                gesture,
                point,
                baseline,
                hid,
                nonce: own,
                ..
            } if *own == nonce => {
                let (gesture, point) = (*gesture, *point);
                let counted = hid.since(baseline.hid);
                let observed = baseline.seen.map(|start| seen - start);
                if observed == Some(counted) {
                    debug!(?gesture, counted, "release confirmed");
                    self.hold = Hold::Idle;
                    out.signals.push(Signal::Release { gesture, point });
                } else {
                    warn!(
                        ?gesture,
                        counted,
                        ?observed,
                        "the tap missed key events of the gesture; cancelling it"
                    );
                    self.fail(out);
                }
            }
            _ => {
                if !self.log.on_marker(nonce, seq) {
                    debug!(nonce, seq, "late or unknown marker swallowed");
                }
            }
        }
    }

    /// Cancels a release whose marker is late.
    fn expire(&mut self, now: Instant, out: &mut Outcome) {
        if let Hold::Releasing {
            gesture, deadline, ..
        } = self.hold
            && now >= deadline
        {
            warn!(?gesture, "release marker is late; cancelling the gesture");
            self.fail(out);
        }
    }

    /// Ends the gesture without an action and marks the log interrupted.
    fn fail(&mut self, out: &mut Outcome) {
        self.log.record_interrupt();
        self.end(out);
    }

    /// Ends the gesture without an action.
    fn end(&mut self, out: &mut Outcome) {
        match std::mem::take(&mut self.hold) {
            Hold::Idle => {}
            Hold::Held { gesture, point, .. } | Hold::Releasing { gesture, point, .. } => {
                out.signals.push(Signal::Cancel { gesture });
                out.signals.push(Signal::Release { gesture, point });
            }
        }
    }
}

#[cfg(test)]
mod tests;
