//! Posting marker events.
//!
//! A marker is a scroll wheel event with zero delta and Orbit's tag in
//! `EventSourceUserData` (spike F2/F11). Posted at the HID level, it enters
//! the same queue as real input, so the tap sees it after every event that
//! was queued before it. The tap swallows every marker; no application sees
//! one.

use objc2_core_graphics::{CGEvent, CGEventField, CGEventTapLocation, CGScrollEventUnit};
use thiserror::Error;

/// Why a marker could not be posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MarkerError {
    /// The system did not create the event.
    #[error("marker event not created")]
    NotCreated,
}

/// Posts a marker carrying `user_data`.
pub fn post_marker(user_data: i64) -> Result<(), MarkerError> {
    let event = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 1, 0, 0, 0)
        .ok_or(MarkerError::NotCreated)?;
    CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, user_data);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    Ok(())
}
