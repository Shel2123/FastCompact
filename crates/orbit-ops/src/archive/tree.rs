//! A walk of the chosen tree that opens nothing by a full path.
//!
//! The root is opened relative to its parent folder without following a
//! link and checked against the target. Every entry below it is classified
//! with `fstatat(AT_SYMLINK_NOFOLLOW)` relative to the descriptor of its
//! folder, then opened relative to that descriptor with `O_NOFOLLOW` and
//! checked to have the type and identity that were classified. A swap
//! between the two steps fails the job with `SourceChanged::Replaced`
//! instead of reading something outside the tree.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orbit_app::messages::JobError;
use orbit_domain::target::{FileIdentity, FileKind, FileTarget, FileVersion, SourceChanged};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::publish::{folder, io_error, quoted};

/// What an entry is, as far as the archive cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Directory,
    Symlink,
    /// A FIFO, socket or device: never opened, never archived.
    Special,
}

/// The `stat` fields the archive uses.
#[derive(Debug, Clone, Copy)]
pub(super) struct Meta {
    pub(super) kind: Kind,
    pub(super) device: u64,
    pub(super) inode: u64,
    /// Permission bits, without the file type.
    pub(super) permissions: u32,
    pub(super) size: u64,
    pub(super) modified: SystemTime,
    /// Whole seconds of `modified` since the epoch, rounded down.
    pub(super) seconds: i64,
}

impl Meta {
    // The field types of `stat` differ between platforms; the casts are the
    // ones std uses for `MetadataExt`.
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &Stat) -> Self {
        let kind = match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => Kind::File,
            FileType::Directory => Kind::Directory,
            FileType::Symlink => Kind::Symlink,
            _ => Kind::Special,
        };
        let seconds = stat.st_mtime as i64;
        let nanos = u32::try_from(stat.st_mtime_nsec as i64)
            .unwrap_or(0)
            .min(999_999_999);
        Self {
            kind,
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            permissions: (stat.st_mode as u32) & 0o7777,
            size: u64::try_from(stat.st_size as i64).unwrap_or(0),
            modified: system_time(seconds, nanos),
            seconds,
        }
    }

    fn same_object(&self, other: &Self) -> bool {
        self.kind == other.kind && self.device == other.device && self.inode == other.inode
    }
}

fn system_time(seconds: i64, nanos: u32) -> SystemTime {
    let whole = Duration::from_secs(seconds.unsigned_abs());
    let time = if seconds >= 0 {
        UNIX_EPOCH.checked_add(whole)
    } else {
        UNIX_EPOCH.checked_sub(whole)
    };
    time.and_then(|time| time.checked_add(Duration::from_nanos(u64::from(nanos))))
        .unwrap_or(UNIX_EPOCH)
}

/// The chosen object, open and checked against the target.
#[derive(Debug)]
pub(super) struct Root {
    /// The file name of the target: the name of its entry in the archive.
    pub(super) name: String,
    pub(super) object: File,
    pub(super) meta: Meta,
}

/// Opens the target relative to its parent folder and checks that it is
/// still the object the gesture chose.
///
/// A path that now holds nothing, a link or something of another type
/// counts as replaced.
pub(super) fn open_root(target: &FileTarget) -> Result<Root, JobError> {
    let (Some(parent), Some(name)) = (target.path.parent(), target.path.file_name()) else {
        return Err(JobError::Internal(
            "the target has no folder or no name".into(),
        ));
    };
    let entry_name = utf8(name, None)?.to_owned();
    let parent_fd = rustix::fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| {
        io_error(
            &io::Error::from(errno),
            &format!("couldn't open {}", folder(parent)),
        )
    })?;
    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if matches!(target.facts.kind, FileKind::Directory | FileKind::Package) {
        flags |= OFlags::DIRECTORY;
    }
    let fd = rustix::fs::openat(&parent_fd, name, flags, Mode::empty()).map_err(|errno| {
        if matches!(errno, Errno::LOOP | Errno::NOTDIR | Errno::NOENT) {
            JobError::from(SourceChanged::Replaced)
        } else {
            read_error(errno, &entry_name)
        }
    })?;
    let object = File::from(fd);
    check_root(target, &object, &entry_name)?;
    let meta = stat_of(&object, &entry_name)?;
    Ok(Root {
        name: entry_name,
        object,
        meta,
    })
}

/// Checks an open root against the target: the identity, size and
/// modification time it had when the gesture read it.
pub(super) fn check_root(target: &FileTarget, object: &File, path: &str) -> Result<(), JobError> {
    let failed = |error: io::Error| io_error(&error, &format!("couldn't read “{path}”"));
    let metadata = object.metadata().map_err(failed)?;
    let identity = FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    let version = FileVersion {
        size: metadata.len(),
        modified: metadata.modified().map_err(failed)?,
    };
    target.check_source(identity, version)?;
    Ok(())
}

/// An entry found by [`Walk::next`].
#[derive(Debug)]
pub(super) enum Entry<'a> {
    /// A regular file, not opened yet: see [`open_file`].
    File(At<'a>),
    /// A folder, already open; the walk lists it next.
    Directory {
        path: String,
        meta: Meta,
        dir: &'a File,
    },
    /// A symbolic link, not read yet: see [`read_link`].
    Symlink(At<'a>),
}

/// An entry by its name in an open folder.
#[derive(Debug)]
pub(super) struct At<'a> {
    /// Path of the entry in the archive, from the root name, with `/`.
    pub(super) path: String,
    /// What `fstatat` said before the entry was opened.
    pub(super) meta: Meta,
    parent: &'a File,
    name: OsString,
}

struct Frame {
    dir: File,
    path: String,
    names: std::vec::IntoIter<OsString>,
}

/// Depth-first walk below an open root folder, entries of each folder in
/// byte order of their names, every folder before what it contains.
///
/// Holds one descriptor per level of the current path.
pub(super) struct Walk<'h> {
    stack: Vec<Frame>,
    device: u64,
    /// Called with the path of every entry between its classification and
    /// its opening; tests use it to swap entries.
    hook: &'h mut dyn FnMut(&str),
    /// FIFOs, sockets and devices that were skipped.
    pub(super) skipped_special: u64,
    /// Entries skipped because they are on another volume than the root.
    pub(super) skipped_other_volume: u64,
}

impl<'h> Walk<'h> {
    /// Starts below `root`, an open folder whose entry is `path`.
    pub(super) fn new(
        root: File,
        path: String,
        meta: &Meta,
        hook: &'h mut dyn FnMut(&str),
    ) -> Result<Self, JobError> {
        let names = list(&root, &path)?;
        Ok(Self {
            stack: vec![Frame {
                dir: root,
                path,
                names: names.into_iter(),
            }],
            device: meta.device,
            hook,
            skipped_special: 0,
            skipped_other_volume: 0,
        })
    }

    /// The next entry, or `None` after the last one.
    ///
    /// # Errors
    ///
    /// `SourceChanged::Replaced` when a folder was swapped for something
    /// else between its classification and its opening; `Io` when an entry
    /// cannot be classified or a folder cannot be opened or listed, or a
    /// name is not UTF-8.
    pub(super) fn next(&mut self) -> Result<Option<Entry<'_>>, JobError> {
        // Decided while the stack is borrowed mutably; the entry borrows it
        // again afterwards.
        let (path, meta, name) = loop {
            let Some(frame) = self.stack.last_mut() else {
                return Ok(None);
            };
            let Some(name) = frame.names.next() else {
                self.stack.pop();
                continue;
            };
            let path = format!("{}/{}", frame.path, utf8(&name, Some(&frame.path))?);
            let stat = rustix::fs::statat(&frame.dir, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|errno| read_error(errno, &path))?;
            let meta = Meta::from_stat(&stat);
            if meta.device != self.device {
                self.skipped_other_volume += 1;
                continue;
            }
            match meta.kind {
                Kind::Special => {
                    self.skipped_special += 1;
                    continue;
                }
                Kind::Directory => {
                    (self.hook)(&path);
                    let dir = open_dir(&frame.dir, &name, &path, &meta)?;
                    let names = list(&dir, &path)?;
                    self.stack.push(Frame {
                        dir,
                        path: path.clone(),
                        names: names.into_iter(),
                    });
                    break (path, meta, None);
                }
                Kind::File | Kind::Symlink => {
                    (self.hook)(&path);
                    break (path, meta, Some(name));
                }
            }
        };
        let Some(frame) = self.stack.last() else {
            return Err(JobError::Internal("the walk lost its folder".into()));
        };
        let entry = match (meta.kind, name) {
            (Kind::Directory, _) => Entry::Directory {
                path,
                meta,
                dir: &frame.dir,
            },
            (Kind::Symlink, Some(name)) => Entry::Symlink(At {
                path,
                meta,
                parent: &frame.dir,
                name,
            }),
            (_, Some(name)) => Entry::File(At {
                path,
                meta,
                parent: &frame.dir,
                name,
            }),
            (_, None) => return Err(JobError::Internal("an entry lost its name".into())),
        };
        Ok(Some(entry))
    }
}

/// Opens a regular file found by the walk and checks that it is still the
/// classified one.
pub(super) fn open_file(at: &At<'_>) -> Result<File, JobError> {
    // O_NONBLOCK: a FIFO swapped in does not block the open; the check below
    // then refuses it.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(at.parent, at.name.as_os_str(), flags, Mode::empty()).map_err(
        |errno| {
            if errno == Errno::LOOP {
                JobError::from(SourceChanged::Replaced)
            } else {
                read_error(errno, &at.path)
            }
        },
    )?;
    let file = File::from(fd);
    if !stat_of(&file, &at.path)?.same_object(&at.meta) {
        return Err(SourceChanged::Replaced.into());
    }
    Ok(file)
}

/// Reads the target of a link found by the walk.
pub(super) fn read_link(at: &At<'_>) -> Result<String, JobError> {
    let target =
        rustix::fs::readlinkat(at.parent, at.name.as_os_str(), Vec::new()).map_err(|errno| {
            // EINVAL: the entry is no longer a link.
            if errno == Errno::INVAL {
                JobError::from(SourceChanged::Replaced)
            } else {
                read_error(errno, &at.path)
            }
        })?;
    target.into_string().map_err(|_| JobError::Io {
        kind: ErrorKind::InvalidData,
        message: format!(
            "the link “{}” points to a name that isn't valid UTF-8",
            at.path
        ),
    })
}

/// `fstat` of an open object.
pub(super) fn stat_of(object: &File, path: &str) -> Result<Meta, JobError> {
    rustix::fs::fstat(object)
        .map(|stat| Meta::from_stat(&stat))
        .map_err(|errno| read_error(errno, path))
}

fn open_dir(parent: &File, name: &OsStr, path: &str, expected: &Meta) -> Result<File, JobError> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(parent, name, flags, Mode::empty()).map_err(|errno| {
        if matches!(errno, Errno::LOOP | Errno::NOTDIR) {
            JobError::from(SourceChanged::Replaced)
        } else {
            read_error(errno, path)
        }
    })?;
    let dir = File::from(fd);
    if !stat_of(&dir, path)?.same_object(expected) {
        return Err(SourceChanged::Replaced.into());
    }
    Ok(dir)
}

/// Names in an open folder, without `.` and `..`, in byte order.
fn list(dir: &File, path: &str) -> Result<Vec<OsString>, JobError> {
    let failed =
        |errno: Errno| io_error(&io::Error::from(errno), &format!("couldn't list “{path}”"));
    let mut names = Vec::new();
    for entry in Dir::read_from(dir).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_os_string());
        }
    }
    names.sort_unstable();
    Ok(names)
}

/// The name as UTF-8, which ZIP entry names need. `folder` is the path of
/// the folder that holds it, for the message.
fn utf8<'n>(name: &'n OsStr, folder: Option<&str>) -> Result<&'n str, JobError> {
    name.to_str().ok_or_else(|| {
        let shown = match folder {
            Some(folder) => format!("“{folder}/{}”", name.to_string_lossy()),
            None => quoted(name),
        };
        JobError::Io {
            kind: ErrorKind::InvalidData,
            message: format!("{shown} has a name that isn't valid UTF-8"),
        }
    })
}

fn read_error(errno: Errno, path: &str) -> JobError {
    io_error(&io::Error::from(errno), &format!("couldn't read “{path}”"))
}
