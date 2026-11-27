//! Reading one folder: what it holds, with the metadata the count needs.
//!
//! [`BulkLister`] gets many entries per call through `getattrlistbulk`;
//! [`StdLister`] gets them one by one through `read_dir` and `lstat`. Both
//! open the folder first and skip it when it lies on another volume.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::ops::ControlFlow;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use tracing::trace;

use super::bulk;

/// What an entry is, as far as the count cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Folder,
    Link,
    /// A socket, a pipe, a device: counted as nothing.
    Other,
}

impl Kind {
    pub(super) fn of(metadata: &Metadata) -> Self {
        let kind = metadata.file_type();
        if kind.is_file() {
            Self::File
        } else if kind.is_dir() {
            Self::Folder
        } else if kind.is_symlink() {
            Self::Link
        } else {
            Self::Other
        }
    }

    /// A `vtype` from `sys/vnode.h`.
    fn of_vtype(vtype: u32) -> Self {
        match vtype {
            1 => Self::File,
            2 => Self::Folder,
            5 => Self::Link,
            _ => Self::Other,
        }
    }

    /// Whether the size on the disk and link count of such an entry matter.
    fn has_data(self) -> bool {
        matches!(self, Self::File | Self::Link)
    }
}

/// One entry of a folder. `on_disk` and `nlink` are 0 and 1 for what is not a
/// file or a symbolic link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) name: OsString,
    pub(super) kind: Kind,
    /// The bytes it takes on the disk: `st_blocks` × 512.
    pub(super) on_disk: u64,
    pub(super) ino: u64,
    pub(super) nlink: u64,
    /// `st_flags`.
    pub(super) flags: u32,
}

impl Entry {
    fn from_metadata(name: OsString, metadata: &Metadata) -> Self {
        let kind = Kind::of(metadata);
        let (on_disk, nlink) = if kind.has_data() {
            (metadata.blocks().saturating_mul(512), metadata.nlink())
        } else {
            (0, 1)
        };
        Self {
            name,
            kind,
            on_disk,
            ino: metadata.ino(),
            nlink,
            flags: metadata.st_flags(),
        }
    }
}

/// What a lister hands over for each entry.
#[derive(Debug)]
pub(super) enum Found {
    Entry(Entry),
    /// An entry whose metadata could not be read.
    Unreadable,
}

/// How the listing of a folder ended.
#[derive(Debug)]
pub(super) enum Listed {
    /// Every entry was handed over.
    Whole,
    /// `each` asked to stop.
    Stopped,
    /// The folder lies on another volume; nothing was read.
    OtherVolume,
    /// Reading failed after some entries were handed over. They stay
    /// counted; the rest of the folder is missing.
    Broken(io::Error),
}

/// Reads folders, one at a time.
pub(super) trait Lister {
    /// Opens `dir`, checks that it lies on `device`, and hands each entry
    /// to `each`. An error means nothing was handed over.
    fn list(
        &mut self,
        dir: &Path,
        device: u64,
        each: &mut dyn FnMut(Found) -> ControlFlow<()>,
    ) -> io::Result<Listed>;
}

/// Opens `dir` as a folder, without following a link in its place.
/// `None` when it lies on another device than `device`: the device of the
/// open folder is the one that counts, a mount point included.
fn open(dir: &Path, device: u64) -> io::Result<Option<File>> {
    let folder = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?;
    let here = folder.metadata()?.dev();
    Ok((here == device).then_some(folder))
}

/// Reads entries one by one: `read_dir`, then `lstat` for each.
pub(super) struct StdLister;

impl Lister for StdLister {
    fn list(
        &mut self,
        dir: &Path,
        device: u64,
        each: &mut dyn FnMut(Found) -> ControlFlow<()>,
    ) -> io::Result<Listed> {
        if open(dir, device)?.is_none() {
            return Ok(Listed::OtherVolume);
        }
        list_std(dir, each)
    }
}

fn list_std(dir: &Path, each: &mut dyn FnMut(Found) -> ControlFlow<()>) -> io::Result<Listed> {
    for entry in fs::read_dir(dir)? {
        // `DirEntry::metadata` does not follow symbolic links.
        let found = entry
            .and_then(|entry| {
                let metadata = entry.metadata()?;
                Ok(Entry::from_metadata(entry.file_name(), &metadata))
            })
            .map_or(Found::Unreadable, Found::Entry);
        if each(found).is_break() {
            return Ok(Listed::Stopped);
        }
    }
    Ok(Listed::Whole)
}

/// The size of the buffer of [`BulkLister`].
const BUF_LEN: usize = 256 * 1024;

/// One call of `getattrlistbulk`: fills the buffer, returns how many
/// records it holds, 0 at the end of the folder.
pub(super) type BulkCall = fn(BorrowedFd<'_>, &mut [u8]) -> io::Result<usize>;

/// Reads many entries per call through `getattrlistbulk`. The buffer is
/// kept between folders.
pub(super) struct BulkLister<C> {
    buf: Vec<u8>,
    call: C,
}

impl BulkLister<BulkCall> {
    pub(super) fn new() -> Self {
        Self::with_call(getattrlistbulk)
    }
}

impl<C> BulkLister<C>
where
    C: FnMut(BorrowedFd<'_>, &mut [u8]) -> io::Result<usize>,
{
    /// A lister whose system call is `call`; the tests pass a fake one.
    pub(super) fn with_call(call: C) -> Self {
        Self {
            buf: vec![0; BUF_LEN],
            call,
        }
    }
}

impl<C> Lister for BulkLister<C>
where
    C: FnMut(BorrowedFd<'_>, &mut [u8]) -> io::Result<usize>,
{
    fn list(
        &mut self,
        dir: &Path,
        device: u64,
        each: &mut dyn FnMut(Found) -> ControlFlow<()>,
    ) -> io::Result<Listed> {
        let Some(folder) = open(dir, device)? else {
            return Ok(Listed::OtherVolume);
        };
        // Until an entry is handed over, a failure is read again the slow
        // way; after that, reading again would count entries twice.
        let mut handed = false;
        loop {
            let count = match (self.call)(folder.as_fd(), &mut self.buf) {
                Ok(0) => return Ok(Listed::Whole),
                Ok(count) => count,
                Err(err) if !handed => {
                    trace!(dir = %dir.display(), %err, "bulk listing failed, reading entry by entry");
                    return list_std(dir, each);
                }
                Err(err) => return Ok(Listed::Broken(err)),
            };
            for record in bulk::records(&self.buf, count) {
                let found = match record {
                    Ok(record) => found(dir, &record),
                    Err(err) if !handed => {
                        trace!(dir = %dir.display(), %err, "bulk record unreadable, reading entry by entry");
                        return list_std(dir, each);
                    }
                    Err(err) => return Ok(Listed::Broken(err)),
                };
                handed = true;
                if each(found).is_break() {
                    return Ok(Listed::Stopped);
                }
            }
        }
    }
}

/// The entry of a record. What the record lacks is read with `lstat`.
fn found(dir: &Path, record: &bulk::Record<'_>) -> Found {
    if record.error != 0 {
        return Found::Unreadable;
    }
    let Some(name) = record.name.filter(|name| !name.is_empty()) else {
        return Found::Unreadable;
    };
    let name = OsStr::from_bytes(name).to_owned();
    if let (Some(kind), Some(flags), Some(ino)) = (record.kind, record.flags, record.ino) {
        let kind = Kind::of_vtype(kind);
        let data = if kind.has_data() {
            record.on_disk.zip(record.nlink.map(u64::from))
        } else {
            Some((0, 1))
        };
        if let Some((on_disk, nlink)) = data {
            return Found::Entry(Entry {
                name,
                kind,
                on_disk,
                ino,
                nlink,
                flags,
            });
        }
    }
    match fs::symlink_metadata(dir.join(&name)) {
        Ok(metadata) => Found::Entry(Entry::from_metadata(name, &metadata)),
        Err(_) => Found::Unreadable,
    }
}

/// The real system call.
fn getattrlistbulk(folder: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<usize> {
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: bulk::COMMON,
        volattr: 0,
        dirattr: 0,
        fileattr: bulk::FILE,
        forkattr: 0,
    };
    // SAFETY: `folder` stays open for the call. `attrs` is a valid
    // `attrlist`, and the kernel writes at most `buf.len()` bytes into
    // `buf`, which is valid for writes of that many.
    let count = unsafe {
        libc::getattrlistbulk(
            folder.as_raw_fd(),
            std::ptr::from_mut(&mut attrs).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
        )
    };
    usize::try_from(count).map_err(|_| io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use super::super::bulk::build::Fields;
    use super::*;
    use crate::platform::test_dir::TempDir;

    /// Every entry of `dir`, sorted by name. Fails on an unreadable entry
    /// or a listing that did not end whole.
    fn entries(lister: &mut impl Lister, dir: &Path) -> io::Result<Vec<Entry>> {
        let device = fs::symlink_metadata(dir)?.dev();
        let mut entries = Vec::new();
        let mut unreadable = 0;
        let listed = lister.list(dir, device, &mut |found| {
            match found {
                Found::Entry(entry) => entries.push(entry),
                Found::Unreadable => unreadable += 1,
            }
            ControlFlow::Continue(())
        })?;
        assert!(matches!(listed, Listed::Whole), "{listed:?}");
        assert_eq!(unreadable, 0);
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// A folder with a file, an empty file, a subfolder, a symbolic link
    /// and a hard link.
    fn sample() -> io::Result<TempDir> {
        let dir = TempDir::new();
        fs::write(dir.join("a"), [0; 10])?;
        fs::write(dir.join("empty"), [])?;
        fs::create_dir(dir.join("sub"))?;
        fs::write(dir.join("sub/x"), [0; 3])?;
        symlink(dir.join("a"), dir.join("link"))?;
        fs::hard_link(dir.join("a"), dir.join("hard"))?;
        // A resource fork, and a file compressed by the file system: their
        // size on the disk differs from the length of the data.
        fs::write(dir.join("forked"), [0; 10])?;
        fs::write(dir.join("forked/..namedfork/rsrc"), vec![1; 100_000])?;
        fs::write(dir.join("plain"), vec![b'x'; 200_000])?;
        let compressed = Command::new("ditto")
            .arg("--hfsCompression")
            .arg(dir.join("plain"))
            .arg(dir.join("compressed"))
            .status()?;
        assert!(compressed.success());
        Ok(dir)
    }

    #[test]
    fn bulk_and_std_see_the_same_entries() -> io::Result<()> {
        let dir = sample()?;
        let bulk = entries(&mut BulkLister::new(), dir.path())?;
        let std = entries(&mut StdLister, dir.path())?;
        assert_eq!(bulk, std);
        let names: Vec<_> = bulk.iter().map(|entry| entry.name.as_os_str()).collect();
        assert_eq!(
            names,
            [
                "a",
                "compressed",
                "empty",
                "forked",
                "hard",
                "link",
                "plain",
                "sub"
            ]
        );
        let [a, compressed, _, forked, hard, link, plain, sub] = bulk.as_slice() else {
            panic!("{bulk:?}");
        };
        assert_eq!((link.kind, sub.kind), (Kind::Link, Kind::Folder));
        assert!(a.on_disk > 0);
        assert_eq!((hard.on_disk, hard.nlink), (a.on_disk, 2));
        assert!(
            compressed.on_disk < plain.on_disk,
            "{compressed:?} {plain:?}"
        );
        assert!(forked.on_disk >= 100_000, "{forked:?}");
        assert_eq!(hard.ino, a.ino);
        Ok(())
    }

    #[test]
    fn the_system_call_returns_every_field() -> io::Result<()> {
        let dir = sample()?;
        let folder = File::open(dir.path())?;
        let mut buf = vec![0; BUF_LEN];
        let mut names = Vec::new();
        loop {
            let count = getattrlistbulk(folder.as_fd(), &mut buf)?;
            if count == 0 {
                break;
            }
            for record in bulk::records(&buf, count) {
                let record = record?;
                assert_eq!(record.error, 0);
                assert!(record.kind.is_some() && record.flags.is_some() && record.ino.is_some());
                if record.kind != Some(2) {
                    assert!(
                        record.nlink.is_some() && record.on_disk.is_some(),
                        "{record:?}"
                    );
                }
                names.extend(record.name.map(<[u8]>::to_vec));
            }
        }
        names.sort();
        assert_eq!(names.len(), 8, "{names:?}");
        Ok(())
    }

    #[test]
    fn a_failing_first_call_reads_the_folder_entry_by_entry() -> io::Result<()> {
        let dir = sample()?;
        let mut lister =
            BulkLister::with_call(|_, _| Err(io::Error::from_raw_os_error(libc::ENOTSUP)));
        assert_eq!(
            entries(&mut lister, dir.path())?,
            entries(&mut StdLister, dir.path())?
        );
        Ok(())
    }

    #[test]
    fn a_broken_first_record_reads_the_folder_entry_by_entry() -> io::Result<()> {
        let dir = sample()?;
        let mut lister = BulkLister::with_call(|_, buf: &mut [u8]| {
            buf[..4].copy_from_slice(&3_u32.to_ne_bytes());
            Ok(1)
        });
        assert_eq!(
            entries(&mut lister, dir.path())?,
            entries(&mut StdLister, dir.path())?
        );
        Ok(())
    }

    #[test]
    fn a_record_without_some_fields_is_read_from_the_disk() -> io::Result<()> {
        let dir = TempDir::new();
        fs::write(dir.join("real"), [0; 1000])?;
        let fields = Fields {
            ino: None,
            on_disk: None,
            ..Fields::file(b"real", 0, 0)
        };
        let buf = fields.record();
        let record = bulk::records(&buf, 1).next().transpose()?;
        let Some(record) = record else {
            panic!("no record");
        };
        let Found::Entry(entry) = found(dir.path(), &record) else {
            panic!("unreadable");
        };
        let metadata = fs::symlink_metadata(dir.join("real"))?;
        assert_eq!(
            (entry.on_disk, entry.ino),
            (metadata.blocks() * 512, metadata.ino())
        );
        assert!(entry.on_disk > 0);
        Ok(())
    }

    #[test]
    fn a_record_with_an_error_or_without_a_name_is_unreadable() -> io::Result<()> {
        let dir = TempDir::new();
        for fields in [
            Fields {
                error: Some(libc::EIO as u32),
                ..Fields::file(b"a", 1, 1)
            },
            Fields {
                name: None,
                ..Fields::file(b"a", 1, 1)
            },
        ] {
            let buf = fields.record();
            for record in bulk::records(&buf, 1) {
                assert!(matches!(found(dir.path(), &record?), Found::Unreadable));
            }
        }
        Ok(())
    }

    #[test]
    fn a_missing_folder_is_an_error() {
        let dir = TempDir::new();
        let missing = dir.join("missing");
        let mut each = |_: Found| ControlFlow::Continue(());
        assert!(StdLister.list(&missing, 0, &mut each).is_err());
        assert!(BulkLister::new().list(&missing, 0, &mut each).is_err());
    }
}
