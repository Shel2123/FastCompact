//! Calendar time for the DOS time field of ZIP entries.

use std::time::{SystemTime, UNIX_EPOCH};

const SECONDS_PER_DAY: i64 = 86_400;

/// A moment as a calendar date and a time of day.
///
/// The platform fills it from its time zone; portable code knows none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// Year, such as 2026.
    pub year: i32,
    /// Month, 1 to 12.
    pub month: u8,
    /// Day of the month, 1 to 31.
    pub day: u8,
    /// Hour, 0 to 23.
    pub hour: u8,
    /// Minute, 0 to 59.
    pub minute: u8,
    /// Second, 0 to 60.
    pub second: u8,
}

impl LocalTime {
    /// The moment in UTC, for a platform that does not know its time zone.
    ///
    /// `None` when the year does not fit in `i32`.
    pub fn utc(time: SystemTime) -> Option<Self> {
        let seconds = match time.duration_since(UNIX_EPOCH) {
            Ok(elapsed) => i64::try_from(elapsed.as_secs()).ok()?,
            Err(before) => {
                let before = before.duration();
                let whole = i64::try_from(before.as_secs()).ok()?;
                // Round down: 0.5 s before the epoch is in second -1.
                if before.subsec_nanos() > 0 {
                    -whole - 1
                } else {
                    -whole
                }
            }
        };
        let (year, month, day) = civil_from_days(seconds.div_euclid(SECONDS_PER_DAY))?;
        let of_day = seconds.rem_euclid(SECONDS_PER_DAY);
        Some(Self {
            year,
            month,
            day,
            hour: u8::try_from(of_day / 3600).ok()?,
            minute: u8::try_from(of_day % 3600 / 60).ok()?,
            second: u8::try_from(of_day % 60).ok()?,
        })
    }
}

/// Year, month and day of a day number counted from 1970-01-01, in the
/// proleptic Gregorian calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> Option<(i32, u8, u8)> {
    let days = days.checked_add(719_468)?;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era.checked_add(era.checked_mul(400)?)? + i64::from(month <= 2);
    Some((
        i32::try_from(year).ok()?,
        u8::try_from(month).ok()?,
        u8::try_from(day).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(seconds: i64) -> SystemTime {
        let magnitude = Duration::from_secs(seconds.unsigned_abs());
        if seconds >= 0 {
            UNIX_EPOCH + magnitude
        } else {
            UNIX_EPOCH - magnitude
        }
    }

    fn time(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> LocalTime {
        LocalTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    #[test]
    fn utc_reads_the_calendar() {
        assert_eq!(LocalTime::utc(at(0)), Some(time(1970, 1, 1, 0, 0, 0)));
        assert_eq!(
            LocalTime::utc(at(1_700_000_000)),
            Some(time(2023, 11, 14, 22, 13, 20))
        );
        // A leap day and the last second of a year.
        assert_eq!(
            LocalTime::utc(at(951_782_400)),
            Some(time(2000, 2, 29, 0, 0, 0))
        );
        assert_eq!(
            LocalTime::utc(at(1_798_761_599)),
            Some(time(2026, 12, 31, 23, 59, 59))
        );
    }

    #[test]
    fn utc_handles_times_before_the_epoch() {
        assert_eq!(LocalTime::utc(at(-1)), Some(time(1969, 12, 31, 23, 59, 59)));
        assert_eq!(
            LocalTime::utc(UNIX_EPOCH - Duration::from_millis(500)),
            Some(time(1969, 12, 31, 23, 59, 59))
        );
        assert_eq!(
            LocalTime::utc(at(-315_619_200)),
            Some(time(1960, 1, 1, 0, 0, 0))
        );
    }
}
