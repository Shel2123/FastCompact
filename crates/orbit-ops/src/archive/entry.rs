//! Names and options of ZIP entries.

use std::time::SystemTime;

use orbit_app::messages::JobError;
use zip::write::{FullFileOptions, SimpleFileOptions};
use zip::{CompressionMethod, DateTime};

use super::time::LocalTime;
use super::tree::Meta;

/// Entries from this size on carry ZIP64 fields.
///
/// A seekable writer needs the flag before the entry starts. The limit is
/// below 4 GiB because deflate makes incompressible data slightly larger.
const LARGE_FILE: u64 = 1 << 31;

/// Options of the entry of an object: deflate, permission bits, the DOS
/// time in local time and the exact time in UTC as an extended timestamp.
pub(super) fn options(
    meta: &Meta,
    local_time: fn(SystemTime) -> Option<LocalTime>,
) -> Result<FullFileOptions<'static, 'static>, JobError> {
    let simple = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(meta.permissions & 0o777)
        .last_modified_time(dos_time(local_time(meta.modified)))
        .large_file(meta.size >= LARGE_FILE);
    // The field holds an unsigned 32-bit count of seconds: until 2106.
    match u32::try_from(meta.seconds) {
        Ok(seconds) => simple
            .with_extended_timestamp(Some(seconds), None, None)
            .map_err(|error| JobError::Internal(format!("ZIP options: {error}"))),
        Err(_) => Ok(simple.into_full_options()),
    }
}

/// Options of the sidecar of an object: those of the object, without ZIP64
/// fields, which a sidecar never needs.
pub(super) fn sidecar_options(
    object: &FullFileOptions<'static, 'static>,
) -> FullFileOptions<'static, 'static> {
    object.clone().large_file(false)
}

/// The DOS time field: local time from 1980 to 2107, clamped at both ends.
pub(super) fn dos_time(local: Option<LocalTime>) -> DateTime {
    let Some(time) = local else {
        return DateTime::default();
    };
    if time.year < 1980 {
        return DateTime::default();
    }
    let Ok(year) = u16::try_from(time.year) else {
        return latest_dos_time();
    };
    if year > 2107 {
        return latest_dos_time();
    }
    DateTime::from_date_and_time(
        year,
        time.month,
        time.day,
        time.hour,
        time.minute,
        time.second,
    )
    .unwrap_or_default()
}

fn latest_dos_time() -> DateTime {
    DateTime::from_date_and_time(2107, 12, 31, 23, 59, 58).unwrap_or_default()
}

/// Where macOS tools put the sidecar of an entry:
/// `__MACOSX/<folder>/._<name>`, or `__MACOSX/._<name>` for the root.
pub(super) fn sidecar_name(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((folder, name)) => format!("__MACOSX/{folder}/._{name}"),
        None => format!("__MACOSX/._{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(year: i32, month: u8, day: u8) -> Option<LocalTime> {
        Some(LocalTime {
            year,
            month,
            day,
            hour: 13,
            minute: 14,
            second: 15,
        })
    }

    fn dos(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> DateTime {
        DateTime::from_date_and_time(year, month, day, hour, minute, second)
            .unwrap_or_else(|_| panic!("{year}-{month}-{day} is out of range"))
    }

    #[test]
    fn dos_time_keeps_times_in_range() {
        assert_eq!(dos_time(local(2026, 11, 28)), dos(2026, 11, 28, 13, 14, 15));
        assert_eq!(dos_time(local(1980, 1, 1)), dos(1980, 1, 1, 13, 14, 15));
    }

    #[test]
    fn dos_time_clamps_both_ends() {
        assert_eq!(dos_time(local(1979, 12, 31)), DateTime::default());
        assert_eq!(dos_time(local(-5, 1, 1)), DateTime::default());
        assert_eq!(dos_time(None), DateTime::default());
        assert_eq!(dos_time(local(2108, 1, 1)), dos(2107, 12, 31, 23, 59, 58));
        assert_eq!(
            dos_time(local(i32::MAX, 1, 1)),
            dos(2107, 12, 31, 23, 59, 58)
        );
    }

    #[test]
    fn sidecars_go_under_macosx() {
        assert_eq!(sidecar_name("Docs"), "__MACOSX/._Docs");
        assert_eq!(sidecar_name("a.txt"), "__MACOSX/._a.txt");
        assert_eq!(sidecar_name("Docs/a.txt"), "__MACOSX/Docs/._a.txt");
        assert_eq!(sidecar_name("Docs/sub/b"), "__MACOSX/Docs/sub/._b");
    }
}
