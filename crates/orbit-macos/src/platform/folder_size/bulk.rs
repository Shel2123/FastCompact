//! Reading the records `getattrlistbulk` writes into its buffer.
//!
//! A record is a `u32` length, the `attribute_set_t` of what was returned,
//! then the attributes, each only when its bit is in that set: ERROR first,
//! then the other common attributes in the order of their bits (NAME as an
//! `attrreference_t`, OBJTYPE, FLAGS, FILEID), then the file attributes
//! (LINKCOUNT, ALLOCSIZE). The name itself lies after them, where its
//! reference points.
//!
//! Nothing is assumed about alignment. A record that does not fit its
//! buffer, or a field or name that does not fit its record, is
//! `InvalidData`, never a panic.

use std::io;

/// `ATTR_CMN_ERROR` from `sys/attr.h`; libc does not have it.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;

/// The common attributes asked for.
pub(super) const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
    | libc::ATTR_CMN_NAME
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_FLAGS
    | libc::ATTR_CMN_FILEID
    | ATTR_CMN_ERROR;

/// The file attributes asked for.
pub(super) const FILE: u32 = libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE;

/// `attribute_set_t`: five `u32` groups (common, volume, dir, file, fork).
const SET_LEN: usize = 20;
/// Where the file group lies in `attribute_set_t`.
const SET_FILE_AT: usize = 12;

/// One entry of a folder as the buffer has it. A field is `None` when the
/// file system did not return it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Record<'a> {
    /// An error reading this entry; zero when there was none.
    pub(super) error: u32,
    /// The name without its terminating NUL.
    pub(super) name: Option<&'a [u8]>,
    /// A `vtype`: 1 file, 2 folder, 5 symbolic link.
    pub(super) kind: Option<u32>,
    /// `st_flags`.
    pub(super) flags: Option<u32>,
    pub(super) ino: Option<u64>,
    pub(super) nlink: Option<u32>,
    /// The bytes the file takes on the disk, all forks together.
    pub(super) on_disk: Option<u64>,
}

/// The records of one buffer, in order. Stops at the first error.
pub(super) struct Records<'a> {
    buf: &'a [u8],
    left: usize,
}

/// The `count` records `getattrlistbulk` wrote at the start of `buf`.
pub(super) fn records(buf: &[u8], count: usize) -> Records<'_> {
    Records { buf, left: count }
}

impl<'a> Iterator for Records<'a> {
    type Item = io::Result<Record<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let parsed = split(self.buf).and_then(|(record, rest)| {
            self.buf = rest;
            parse(record)
        });
        if parsed.is_err() {
            self.left = 0;
        }
        Some(parsed)
    }
}

/// Splits off the first record of `buf`.
fn split(buf: &[u8]) -> io::Result<(&[u8], &[u8])> {
    let len = Cursor::new(buf).u32()?;
    let len = usize::try_from(len).map_err(|_| invalid("record length"))?;
    if len < 4 + SET_LEN || len > buf.len() {
        return Err(invalid("record length"));
    }
    Ok(buf.split_at(len))
}

fn parse(record: &[u8]) -> io::Result<Record<'_>> {
    let mut cursor = Cursor::new(record);
    cursor.at = 4;
    let common = cursor.u32()?;
    let rest = [cursor.u32()?, cursor.u32()?];
    let file = cursor.u32()?;
    let fork = cursor.u32()?;
    debug_assert_eq!(cursor.at, 4 + SET_FILE_AT + 8);
    // A bit that was not asked for would shift every field after it.
    if common & !COMMON != 0 || file & !FILE != 0 || rest != [0, 0] || fork != 0 {
        return Err(invalid("attributes that were not asked for"));
    }
    let mut out = Record::default();
    if common & ATTR_CMN_ERROR != 0 {
        out.error = cursor.u32()?;
    }
    if common & libc::ATTR_CMN_NAME != 0 {
        out.name = Some(cursor.name()?);
    }
    if common & libc::ATTR_CMN_OBJTYPE != 0 {
        out.kind = Some(cursor.u32()?);
    }
    if common & libc::ATTR_CMN_FLAGS != 0 {
        out.flags = Some(cursor.u32()?);
    }
    if common & libc::ATTR_CMN_FILEID != 0 {
        out.ino = Some(cursor.u64()?);
    }
    if file & libc::ATTR_FILE_LINKCOUNT != 0 {
        out.nlink = Some(cursor.u32()?);
    }
    if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        let size = i64::from_ne_bytes(cursor.take()?);
        out.on_disk = Some(u64::try_from(size).map_err(|_| invalid("negative size"))?);
    }
    Ok(out)
}

/// Reads fields one after another from a record.
struct Cursor<'a> {
    record: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(record: &'a [u8]) -> Self {
        Self { record, at: 0 }
    }

    fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let bytes = self
            .at
            .checked_add(N)
            .and_then(|end| self.record.get(self.at..end))
            .ok_or_else(|| invalid("field past the record"))?;
        let bytes = <[u8; N]>::try_from(bytes).map_err(|_| invalid("field size"))?;
        self.at += N;
        Ok(bytes)
    }

    fn u32(&mut self) -> io::Result<u32> {
        self.take().map(u32::from_ne_bytes)
    }

    fn u64(&mut self) -> io::Result<u64> {
        self.take().map(u64::from_ne_bytes)
    }

    /// An `attrreference_t`: the offset of the data from the reference
    /// itself, then the length of the data with its NUL.
    fn name(&mut self) -> io::Result<&'a [u8]> {
        let reference = self.at;
        let offset = i32::from_ne_bytes(self.take()?);
        let len = self.u32()?;
        let from = usize::try_from(offset)
            .ok()
            .and_then(|offset| reference.checked_add(offset));
        let len = usize::try_from(len).ok();
        let bytes = from
            .zip(len)
            .and_then(|(from, len)| Some(from..from.checked_add(len)?))
            .and_then(|range| self.record.get(range))
            .ok_or_else(|| invalid("name past the record"))?;
        Ok(bytes.split(|&byte| byte == 0).next().unwrap_or_default())
    }
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("getattrlistbulk record: {what}"),
    )
}

/// Builds records the way the kernel lays them out, for the tests.
#[cfg(test)]
pub(super) mod build {
    use super::*;

    /// What goes into one record; `None` leaves the field out.
    #[derive(Debug, Default, Clone)]
    pub(in crate::platform::folder_size) struct Fields<'a> {
        pub(in crate::platform::folder_size) error: Option<u32>,
        pub(in crate::platform::folder_size) name: Option<&'a [u8]>,
        pub(in crate::platform::folder_size) kind: Option<u32>,
        pub(in crate::platform::folder_size) flags: Option<u32>,
        pub(in crate::platform::folder_size) ino: Option<u64>,
        pub(in crate::platform::folder_size) nlink: Option<u32>,
        pub(in crate::platform::folder_size) on_disk: Option<u64>,
    }

    impl<'a> Fields<'a> {
        /// A regular file with every field.
        pub(in crate::platform::folder_size) fn file(
            name: &'a [u8],
            ino: u64,
            on_disk: u64,
        ) -> Self {
            Self {
                error: None,
                name: Some(name),
                kind: Some(1),
                flags: Some(0),
                ino: Some(ino),
                nlink: Some(1),
                on_disk: Some(on_disk),
            }
        }

        pub(in crate::platform::folder_size) fn record(&self) -> Vec<u8> {
            let mut common = libc::ATTR_CMN_RETURNED_ATTRS;
            let mut file = 0;
            let mut fixed = Vec::new();
            if let Some(error) = self.error {
                common |= ATTR_CMN_ERROR;
                fixed.extend(error.to_ne_bytes());
            }
            // The reference is patched once the fixed part is known.
            let reference = self.name.map(|_| {
                common |= libc::ATTR_CMN_NAME;
                let at = fixed.len();
                fixed.extend([0; 8]);
                at
            });
            if let Some(kind) = self.kind {
                common |= libc::ATTR_CMN_OBJTYPE;
                fixed.extend(kind.to_ne_bytes());
            }
            if let Some(flags) = self.flags {
                common |= libc::ATTR_CMN_FLAGS;
                fixed.extend(flags.to_ne_bytes());
            }
            if let Some(ino) = self.ino {
                common |= libc::ATTR_CMN_FILEID;
                fixed.extend(ino.to_ne_bytes());
            }
            if let Some(nlink) = self.nlink {
                file |= libc::ATTR_FILE_LINKCOUNT;
                fixed.extend(nlink.to_ne_bytes());
            }
            if let Some(on_disk) = self.on_disk {
                file |= libc::ATTR_FILE_ALLOCSIZE;
                fixed.extend(i64::try_from(on_disk).unwrap_or(i64::MAX).to_ne_bytes());
            }
            if let (Some(at), Some(name)) = (reference, self.name) {
                let offset = fixed.len() - at;
                let len = name.len() + 1;
                fixed[at..at + 4].copy_from_slice(&(offset as i32).to_ne_bytes());
                fixed[at + 4..at + 8].copy_from_slice(&(len as u32).to_ne_bytes());
                fixed.extend(name);
                fixed.push(0);
            }
            while fixed.len() % 8 != 0 {
                fixed.push(0);
            }
            let mut record = Vec::new();
            record.extend(((4 + SET_LEN + fixed.len()) as u32).to_ne_bytes());
            for group in [common, 0, 0, file, 0] {
                record.extend(group.to_ne_bytes());
            }
            record.extend(fixed);
            record
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build::Fields;
    use super::*;

    fn all(buf: &[u8], count: usize) -> io::Result<Vec<Record<'_>>> {
        records(buf, count).collect()
    }

    fn kind(result: io::Result<Vec<Record<'_>>>) -> Option<io::ErrorKind> {
        result.err().map(|err| err.kind())
    }

    #[test]
    fn records_follow_one_another() -> io::Result<()> {
        let mut buf = Fields::file(b"a", 7, 10).record();
        buf.extend(Fields::file(b"longer name", 8, 20).record());
        buf.extend([0xAA; 64]);
        let records = all(&buf, 2)?;
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0],
            Record {
                error: 0,
                name: Some(b"a".as_slice()),
                kind: Some(1),
                flags: Some(0),
                ino: Some(7),
                nlink: Some(1),
                on_disk: Some(10),
            }
        );
        assert_eq!(records[1].name, Some(b"longer name".as_slice()));
        assert_eq!(records[1].on_disk, Some(20));
        Ok(())
    }

    #[test]
    fn a_name_may_be_any_bytes() -> io::Result<()> {
        let name = [0xFF, 0xFE, b'a'];
        let buf = Fields::file(&name, 1, 1).record();
        assert_eq!(all(&buf, 1)?[0].name, Some(name.as_slice()));
        Ok(())
    }

    #[test]
    fn an_error_comes_with_the_record() -> io::Result<()> {
        let fields = Fields {
            error: Some(libc::EACCES as u32),
            name: Some(b"closed"),
            ..Fields::default()
        };
        let buf = fields.record();
        let record = &all(&buf, 1)?[0];
        assert_eq!(record.error, libc::EACCES as u32);
        assert_eq!(record.name, Some(b"closed".as_slice()));
        Ok(())
    }

    #[test]
    fn fields_left_out_are_none() -> io::Result<()> {
        let fields = Fields {
            ino: None,
            on_disk: None,
            ..Fields::file(b"a", 1, 1)
        };
        let buf = fields.record();
        let record = &all(&buf, 1)?[0];
        assert_eq!((record.ino, record.on_disk), (None, None));
        assert_eq!((record.kind, record.nlink), (Some(1), Some(1)));
        Ok(())
    }

    #[test]
    fn a_record_past_the_buffer_is_invalid() {
        let buf = Fields::file(b"a", 1, 1).record();
        assert_eq!(
            kind(all(&buf[..buf.len() - 1], 1)),
            Some(io::ErrorKind::InvalidData)
        );
        // A second record that is not there.
        assert_eq!(kind(all(&buf, 2)), Some(io::ErrorKind::InvalidData));
    }

    #[test]
    fn a_length_past_the_buffer_is_invalid() {
        let mut buf = Fields::file(b"a", 1, 1).record();
        let len = buf.len() as u32 + 8;
        buf[..4].copy_from_slice(&len.to_ne_bytes());
        assert_eq!(kind(all(&buf, 1)), Some(io::ErrorKind::InvalidData));
    }

    #[test]
    fn a_field_past_the_record_is_invalid() {
        // The set says every field is there, the record ends after it.
        let mut buf = Vec::new();
        buf.extend(((4 + SET_LEN) as u32).to_ne_bytes());
        for group in [COMMON, 0, 0, FILE, 0] {
            buf.extend(group.to_ne_bytes());
        }
        assert_eq!(kind(all(&buf, 1)), Some(io::ErrorKind::InvalidData));
    }

    #[test]
    fn a_name_past_the_record_is_invalid() {
        let mut buf = Fields::file(b"a", 1, 1).record();
        // The reference lies right after the set; point it far away.
        let at = 4 + SET_LEN;
        buf[at..at + 4].copy_from_slice(&1000_i32.to_ne_bytes());
        assert_eq!(kind(all(&buf, 1)), Some(io::ErrorKind::InvalidData));
    }

    #[test]
    fn an_attribute_not_asked_for_is_invalid() {
        let mut buf = Fields::file(b"a", 1, 1).record();
        let devid = libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_DEVID;
        buf[4..8].copy_from_slice(&devid.to_ne_bytes());
        assert_eq!(kind(all(&buf, 1)), Some(io::ErrorKind::InvalidData));
    }

    #[test]
    fn records_stop_after_an_error() {
        let mut buf = Fields::file(b"a", 1, 1).record();
        buf[..4].copy_from_slice(&3_u32.to_ne_bytes());
        let mut records = records(&buf, 5);
        assert!(matches!(records.next(), Some(Err(_))));
        assert!(records.next().is_none());
    }
}
