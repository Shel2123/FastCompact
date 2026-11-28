//! Local time for the DOS time field of ZIP entries.

use std::mem::MaybeUninit;
use std::time::{SystemTime, UNIX_EPOCH};

use orbit_ops::archive::LocalTime;

/// `time` in the time zone of the system, as `localtime_r` gives it.
///
/// `None` when the time does not fit in `time_t` or the system can't
/// convert it.
pub fn local_time(time: SystemTime) -> Option<LocalTime> {
    let seconds = match time.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_secs()).ok()?,
        Err(before) => {
            let before = before.duration();
            let whole = i64::try_from(before.as_secs()).ok()?;
            // Round down: 0.5 s before the epoch is in second -1.
            if before.subsec_nanos() > 0 {
                whole.checked_neg()?.checked_sub(1)?
            } else {
                whole.checked_neg()?
            }
        }
    };
    let seconds = libc::time_t::try_from(seconds).ok()?;
    let mut tm = MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: both pointers are valid for the call; localtime_r fills `tm`
    // and returns null on failure.
    if unsafe { libc::localtime_r(&seconds, tm.as_mut_ptr()) }.is_null() {
        return None;
    }
    // SAFETY: zeroed is a valid `tm`, and localtime_r succeeded.
    let tm = unsafe { tm.assume_init() };
    let part = |value: libc::c_int| u8::try_from(value).ok();
    Some(LocalTime {
        year: tm.tm_year.checked_add(1900)?,
        month: part(tm.tm_mon.checked_add(1)?)?,
        day: part(tm.tm_mday)?,
        hour: part(tm.tm_hour)?,
        minute: part(tm.tm_min)?,
        second: part(tm.tm_sec)?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The time zone of the machine is unknown, but every zone is a whole
    /// number of quarter hours away from UTC: seconds and the minute within
    /// the quarter hour match.
    #[test]
    fn local_time_is_utc_shifted_by_quarter_hours() {
        let moments = [
            UNIX_EPOCH,
            UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            UNIX_EPOCH + Duration::from_secs(1_798_761_599),
            UNIX_EPOCH - Duration::from_millis(500),
            SystemTime::now(),
        ];
        for moment in moments {
            let local = local_time(moment);
            let utc = LocalTime::utc(moment);
            let (Some(local), Some(utc)) = (local, utc) else {
                panic!("{moment:?}: {local:?} {utc:?}");
            };
            assert_eq!(local.second, utc.second, "{moment:?}");
            assert_eq!(local.minute % 15, utc.minute % 15, "{moment:?}");
            assert!((1..=12).contains(&local.month), "{local:?}");
            assert!((1..=31).contains(&local.day), "{local:?}");
            assert!(local.hour < 24, "{local:?}");
            assert!(local.year.abs_diff(utc.year) <= 1, "{local:?} {utc:?}");
        }
    }
}
