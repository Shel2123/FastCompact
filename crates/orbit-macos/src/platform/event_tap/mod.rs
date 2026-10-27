//! The event tap: the only source of gesture events.
//!
//! The tap sees every keyboard and mouse event before the applications do.
//! [`tracker::GestureTracker`] turns them into gesture signals on the tap
//! thread and decides which events to swallow; [`log::InputLog`] keeps the
//! order of the events for the selection guard of the target resolver.
//!
//! Both are free of Core Graphics types, so the logic is tested without a
//! tap. [`tap::EventTap`] runs the real tap and [`fence`] posts markers.

pub mod fence;
pub mod log;
pub mod tap;
pub mod tracker;

/// Tag in the upper half of `EventSourceUserData` that marks an event posted
/// by Orbit itself. The tap swallows every event with this tag.
pub const MARKER_TAG: i64 = 0x0B17;

/// The `EventSourceUserData` of the marker with `nonce`.
pub const fn marker_user_data(nonce: u32) -> i64 {
    (MARKER_TAG << 32) | nonce as i64
}

/// The nonce of a marker, or `None` if `user_data` does not carry
/// [`MARKER_TAG`].
pub fn marker_nonce(user_data: i64) -> Option<u32> {
    if user_data >> 32 != MARKER_TAG {
        return None;
    }
    u32::try_from(user_data & i64::from(u32::MAX)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_user_data_round_trips() {
        for nonce in [0, 1, 0x7FFF_FFFF, u32::MAX] {
            assert_eq!(marker_nonce(marker_user_data(nonce)), Some(nonce));
        }
        assert_eq!(marker_nonce(0), None);
        assert_eq!(marker_nonce(42), None);
        assert_eq!(marker_nonce(-1), None);
        assert_eq!(marker_nonce((MARKER_TAG + 1) << 32), None);
    }
}
