//! Order of the events seen by the tap, for the selection guard.
//!
//! Every event the tap sees, and every interruption of its watch, gets a
//! number from one counter. The target resolver compares numbers, never
//! clock times: an input after the press has a larger number than the press,
//! whatever the threads did meanwhile.
//!
//! # Fence
//!
//! Reading the selection takes time, and the tap may still have events in
//! its queue when the read ends. So after the read the resolver registers a
//! fence, posts a marker event with its nonce and waits until the tap sees
//! the marker. Events are delivered in order, so when the marker is seen,
//! every input that came before the end of the read is recorded; the
//! snapshot taken at the marker is what [`InputLog::check`] judges.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use orbit_app::messages::GestureId;
use thiserror::Error;

use super::marker_user_data;

/// Presses kept for [`InputLog::check`]; older ones are
/// [`Guard::UnknownPress`].
const MAX_PRESSES: usize = 16;

/// Fences waiting at the same time.
const MAX_FENCES: usize = 16;

/// What the tap knew about the moment of a press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Press {
    /// Number of the key-down of the hotkey.
    pub seq: u64,
    /// A mouse button was down at the press, so a drag may change the
    /// selection without any event the guard counts.
    pub button_held: bool,
}

/// The log as the tap saw it at a fence marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FenceSnapshot {
    /// Number of the marker.
    pub seq: u64,
    /// Number of the last input before the marker.
    pub last_input_seq: u64,
    /// Number of the last interruption before the marker.
    pub last_interrupt_seq: u64,
}

/// How a fence ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceOutcome {
    /// The tap saw the marker.
    Seen(FenceSnapshot),
    /// The marker was not seen in time; nothing is known about the input.
    TimedOut,
}

/// Why a fence could not be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FenceError {
    /// Too many fences wait at the same time.
    #[error("too many input checks are running")]
    Full,
}

/// Verdict of the selection guard for one press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// No input between the press and the fence.
    Clean,
    /// A key or mouse input came after the press.
    Inputs,
    /// A mouse button was down at the press.
    ButtonHeld,
    /// The tap may have missed events after the press.
    Interrupted,
    /// The press is not in the log, such as a press too old to be kept.
    UnknownPress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fence {
    Pending,
    Seen(FenceSnapshot),
}

#[derive(Debug, Default)]
struct State {
    seq: u64,
    last_input_seq: u64,
    last_interrupt_seq: u64,
    hotkey_downs: u64,
    hotkey_matched: u64,
    presses: VecDeque<(GestureId, Press)>,
    fences: Vec<(u32, Fence)>,
    next_nonce: u32,
}

impl State {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn next_nonce(&mut self) -> u32 {
        let nonce = self.next_nonce;
        self.next_nonce = nonce.wrapping_add(1);
        nonce
    }

    fn fence(&mut self, nonce: u32) -> Option<&mut Fence> {
        self.fences
            .iter_mut()
            .find(|(own, _)| *own == nonce)
            .map(|(_, fence)| fence)
    }

    fn remove_fence(&mut self, nonce: u32) -> Option<Fence> {
        let index = self.fences.iter().position(|(own, _)| *own == nonce)?;
        Some(self.fences.swap_remove(index).1)
    }
}

/// Numbered log of the events seen by the tap, shared by the tap thread,
/// the main thread and the resolver thread.
///
/// The tap thread records events, presses and markers; the hotkey adapter
/// matches Carbon presses with [`match_hotkey`](Self::match_hotkey); the
/// resolver waits for fences and asks [`check`](Self::check). Everything
/// sits behind one mutex, so numbers grow in the order of the calls.
#[derive(Debug, Default)]
pub struct InputLog {
    state: Mutex<State>,
    fence_seen: Condvar,
}

impl InputLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays consistent between statements, so a panic in
        // another holder leaves nothing half done.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Gives the next number to an event the tap sees.
    pub fn next_seq(&self) -> u64 {
        self.lock().next_seq()
    }

    /// Records that event `seq` is an input that may change the selection.
    pub fn record_input(&self, seq: u64) {
        let mut state = self.lock();
        state.last_input_seq = state.last_input_seq.max(seq);
    }

    /// Records that the tap may have missed events from now on, and returns
    /// the number given to the interruption.
    pub fn record_interrupt(&self) -> u64 {
        let mut state = self.lock();
        let seq = state.next_seq();
        state.last_interrupt_seq = seq;
        seq
    }

    /// Records the key-down of the hotkey that started `gesture`.
    pub fn record_press(&self, gesture: GestureId, press: Press) {
        let mut state = self.lock();
        if state.presses.len() == MAX_PRESSES {
            state.presses.pop_front();
        }
        state.presses.push_back((gesture, press));
        state.hotkey_downs += 1;
    }

    /// Matches one Carbon hotkey press with a key-down the tap recorded.
    ///
    /// The tap sees the key-down before Carbon reports it, so a Carbon
    /// press finds its key-down already counted. Returns `false` when the
    /// tap saw no unmatched key-down, such as under secure keyboard entry.
    pub fn match_hotkey(&self) -> bool {
        let mut state = self.lock();
        if state.hotkey_downs > state.hotkey_matched {
            state.hotkey_matched += 1;
            true
        } else {
            false
        }
    }

    /// A nonce for a marker the tap posts for itself.
    pub fn next_nonce(&self) -> u32 {
        self.lock().next_nonce()
    }

    /// Registers a fence before its marker is posted, so a marker seen
    /// before [`FenceTicket::wait`] is not lost.
    pub fn register_fence(&self) -> Result<FenceTicket<'_>, FenceError> {
        let mut state = self.lock();
        if state.fences.len() == MAX_FENCES {
            return Err(FenceError::Full);
        }
        let nonce = state.next_nonce();
        state.fences.push((nonce, Fence::Pending));
        Ok(FenceTicket { log: self, nonce })
    }

    /// Records that the tap saw marker `nonce` as event `seq`. Returns
    /// whether a fence waited for it; a late or unknown marker changes
    /// nothing.
    pub fn on_marker(&self, nonce: u32, seq: u64) -> bool {
        let mut state = self.lock();
        let snapshot = FenceSnapshot {
            seq,
            last_input_seq: state.last_input_seq,
            last_interrupt_seq: state.last_interrupt_seq,
        };
        let Some(fence) = state.fence(nonce) else {
            return false;
        };
        if *fence != Fence::Pending {
            return false;
        }
        *fence = Fence::Seen(snapshot);
        drop(state);
        self.fence_seen.notify_all();
        true
    }

    /// Judges the selection read for `gesture` by the log at the fence.
    pub fn check(&self, gesture: GestureId, snapshot: &FenceSnapshot) -> Guard {
        let state = self.lock();
        let Some(press) = state
            .presses
            .iter()
            .rev()
            .find(|(own, _)| *own == gesture)
            .map(|(_, press)| *press)
        else {
            return Guard::UnknownPress;
        };
        if press.button_held {
            Guard::ButtonHeld
        } else if snapshot.last_input_seq > press.seq {
            Guard::Inputs
        } else if snapshot.last_interrupt_seq > press.seq {
            Guard::Interrupted
        } else {
            Guard::Clean
        }
    }
}

/// A registered fence. Dropping it without [`wait`](Self::wait) forgets the
/// fence, so a marker that failed to post leaves nothing behind.
#[derive(Debug)]
pub struct FenceTicket<'a> {
    log: &'a InputLog,
    nonce: u32,
}

impl FenceTicket<'_> {
    /// The `EventSourceUserData` of the marker to post.
    pub const fn user_data(&self) -> i64 {
        marker_user_data(self.nonce)
    }

    /// Waits until the tap sees the marker, at most `timeout`.
    pub fn wait(self, timeout: Duration) -> FenceOutcome {
        let state = self.log.lock();
        let (mut state, _) = self
            .log
            .fence_seen
            .wait_timeout_while(state, timeout, |state| {
                matches!(state.fence(self.nonce), Some(Fence::Pending))
            })
            .unwrap_or_else(PoisonError::into_inner);
        match state.remove_fence(self.nonce) {
            Some(Fence::Seen(snapshot)) => FenceOutcome::Seen(snapshot),
            Some(Fence::Pending) | None => FenceOutcome::TimedOut,
        }
    }
}

impl Drop for FenceTicket<'_> {
    fn drop(&mut self) {
        self.log.lock().remove_fence(self.nonce);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::super::marker_nonce;
    use super::*;

    const SHORT: Duration = Duration::from_millis(20);

    fn press(log: &InputLog, gesture: u64) -> u64 {
        let seq = log.next_seq();
        log.record_press(
            GestureId(gesture),
            Press {
                seq,
                button_held: false,
            },
        );
        seq
    }

    fn input(log: &InputLog) {
        let seq = log.next_seq();
        log.record_input(seq);
    }

    /// Registers a fence and lets the tap see its marker at once.
    fn fence(log: &InputLog) -> FenceSnapshot {
        let ticket = match log.register_fence() {
            Ok(ticket) => ticket,
            Err(err) => panic!("fence: {err}"),
        };
        let nonce = marker_nonce(ticket.user_data());
        let seq = log.next_seq();
        assert_eq!(nonce.map(|nonce| log.on_marker(nonce, seq)), Some(true));
        match ticket.wait(SHORT) {
            FenceOutcome::Seen(snapshot) => snapshot,
            FenceOutcome::TimedOut => panic!("fence timed out"),
        }
    }

    #[test]
    fn each_press_is_judged_by_its_own_number() {
        let log = InputLog::new();
        press(&log, 1);
        assert_eq!(log.check(GestureId(1), &fence(&log)), Guard::Clean);

        press(&log, 2);
        input(&log);
        assert_eq!(log.check(GestureId(2), &fence(&log)), Guard::Inputs);

        press(&log, 3);
        assert_eq!(log.check(GestureId(3), &fence(&log)), Guard::Clean);
        // The input of gesture 2 came before the press of gesture 3.
        assert_eq!(log.check(GestureId(2), &fence(&log)), Guard::Inputs);
    }

    #[test]
    fn input_before_the_marker_counts_and_after_it_does_not() {
        let log = InputLog::new();
        press(&log, 1);
        let ticket = log.register_fence();
        let Ok(ticket) = ticket else {
            panic!("fence not registered")
        };
        input(&log);
        let nonce = marker_nonce(ticket.user_data()).unwrap_or_default();
        let seq = log.next_seq();
        assert!(log.on_marker(nonce, seq));
        input(&log);
        match ticket.wait(SHORT) {
            FenceOutcome::Seen(snapshot) => {
                assert_eq!(log.check(GestureId(1), &snapshot), Guard::Inputs);
            }
            FenceOutcome::TimedOut => panic!("fence timed out"),
        }

        press(&log, 2);
        let snapshot = fence(&log);
        input(&log);
        assert_eq!(log.check(GestureId(2), &snapshot), Guard::Clean);
    }

    #[test]
    fn fence_without_its_marker_times_out() {
        let log = InputLog::new();
        let Ok(ticket) = log.register_fence() else {
            panic!("fence not registered")
        };
        assert_eq!(ticket.wait(SHORT), FenceOutcome::TimedOut);
    }

    #[test]
    fn fence_wakes_when_the_marker_comes_from_another_thread() {
        let log = Arc::new(InputLog::new());
        let Ok(ticket) = log.register_fence() else {
            panic!("fence not registered")
        };
        let nonce = marker_nonce(ticket.user_data()).unwrap_or_default();
        let tap = {
            let log = Arc::clone(&log);
            thread::spawn(move || {
                thread::sleep(SHORT);
                let seq = log.next_seq();
                log.on_marker(nonce, seq)
            })
        };
        assert!(matches!(
            ticket.wait(Duration::from_secs(5)),
            FenceOutcome::Seen(_)
        ));
        assert!(matches!(tap.join(), Ok(true)));
    }

    #[test]
    fn two_fences_are_independent() {
        let log = InputLog::new();
        let (Ok(first), Ok(second)) = (log.register_fence(), log.register_fence()) else {
            panic!("fences not registered")
        };
        assert_ne!(first.user_data(), second.user_data());
        let nonce = marker_nonce(second.user_data()).unwrap_or_default();
        let seq = log.next_seq();
        assert!(log.on_marker(nonce, seq));
        assert_eq!(first.wait(SHORT), FenceOutcome::TimedOut);
        assert!(matches!(second.wait(SHORT), FenceOutcome::Seen(s) if s.seq == seq));
    }

    #[test]
    fn late_and_unknown_markers_change_nothing() {
        let log = InputLog::new();
        let Ok(ticket) = log.register_fence() else {
            panic!("fence not registered")
        };
        let nonce = marker_nonce(ticket.user_data()).unwrap_or_default();
        assert_eq!(ticket.wait(SHORT), FenceOutcome::TimedOut);
        let seq = log.next_seq();
        assert!(!log.on_marker(nonce, seq));
        assert!(!log.on_marker(nonce.wrapping_add(100), seq));
        // A marker seen twice counts once.
        let Ok(ticket) = log.register_fence() else {
            panic!("fence not registered")
        };
        let nonce = marker_nonce(ticket.user_data()).unwrap_or_default();
        assert!(log.on_marker(nonce, log.next_seq()));
        assert!(!log.on_marker(nonce, log.next_seq()));
        assert!(matches!(ticket.wait(SHORT), FenceOutcome::Seen(_)));
    }

    #[test]
    fn dropped_ticket_frees_its_place() {
        let log = InputLog::new();
        let tickets: Vec<_> = (0..MAX_FENCES)
            .map(|_| log.register_fence())
            .collect::<Result<_, _>>()
            .unwrap_or_default();
        assert_eq!(tickets.len(), MAX_FENCES);
        assert!(matches!(log.register_fence(), Err(FenceError::Full)));
        drop(tickets);
        assert!(log.register_fence().is_ok());
    }

    #[test]
    fn button_held_and_interrupt_spoil_the_press() {
        let log = InputLog::new();
        let seq = log.next_seq();
        log.record_press(
            GestureId(1),
            Press {
                seq,
                button_held: true,
            },
        );
        assert_eq!(log.check(GestureId(1), &fence(&log)), Guard::ButtonHeld);

        press(&log, 2);
        log.record_interrupt();
        assert_eq!(log.check(GestureId(2), &fence(&log)), Guard::Interrupted);

        // An interrupt right after the press, with no event in between,
        // still gets a larger number.
        press(&log, 3);
        let snapshot = fence(&log);
        let late = log.record_interrupt();
        assert_eq!(log.check(GestureId(3), &snapshot), Guard::Clean);
        press(&log, 4);
        assert!(late < log.next_seq());
        log.record_interrupt();
        assert_eq!(log.check(GestureId(4), &fence(&log)), Guard::Interrupted);
    }

    #[test]
    fn old_presses_are_forgotten() {
        let log = InputLog::new();
        for gesture in 1..=17 {
            press(&log, gesture);
        }
        let snapshot = fence(&log);
        assert_eq!(log.check(GestureId(1), &snapshot), Guard::UnknownPress);
        assert_eq!(log.check(GestureId(2), &snapshot), Guard::Clean);
        assert_eq!(log.check(GestureId(17), &snapshot), Guard::Clean);
        assert_eq!(log.check(GestureId(99), &snapshot), Guard::UnknownPress);
    }

    #[test]
    fn carbon_presses_match_tap_key_downs_one_to_one() {
        let log = InputLog::new();
        assert!(!log.match_hotkey());
        press(&log, 1);
        press(&log, 2);
        assert!(log.match_hotkey());
        assert!(log.match_hotkey());
        assert!(!log.match_hotkey());
        press(&log, 3);
        assert!(log.match_hotkey());
    }
}
