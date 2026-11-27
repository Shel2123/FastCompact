//! The size of a folder: the space what it holds takes on the disk, like
//! `du -x`. A sparse file (a virtual machine disk) counts with the blocks it
//! uses, not with its length: lengths can add up to more than the disk.
//! Clones that share blocks count each, as in `du`.
//!
//! - Symbolic links count with their own blocks and are not followed.
//! - A file with several hard links counts once.
//! - Folders on another volume are not entered, like `du -x`: a folder is
//!   opened first, and skipped when the open folder lies on another device.
//! - iCloud content that is not on the disk is not downloaded: a dataless
//!   folder is counted as unreadable, and the walk threads read with
//!   downloads turned off ([`iopolicy`]). The size is then shown as
//!   "At least …". A dataless file takes nothing on the disk.
//! - What cannot be read is counted as unreadable and skipped.
//!
//! The walk reads the disk and may take long, so it runs off the main
//! thread. Up to [`MAX_WORKERS`] threads read folders from a shared queue;
//! the calling thread reports the running sum. It stops at the next entry
//! once `cancel` is raised; a single call that blocks (a slow network
//! volume) is not interrupted.

mod bulk;
mod iopolicy;
mod listing;

use std::collections::HashSet;
use std::fs;
use std::num::NonZero;
use std::ops::ControlFlow;
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{trace, warn};

use listing::{BulkLister, Entry, Found, Kind, Listed, Lister, StdLister};

/// More threads barely help: 8 are about 15 % faster than 4.
const MAX_WORKERS: usize = 4;

/// How many entries a thread counts before adding them to the shared sum.
const FLUSH_EVERY: u32 = 256;

/// `SF_DATALESS` from `sys/stat.h`; libc does not have it.
const SF_DATALESS: u32 = 0x4000_0000;

/// The result of a whole walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FolderSize {
    /// The space the files take on the disk, in bytes.
    pub bytes: u64,
    /// Folders and entries that could not be read; their size is missing,
    /// so `bytes` is a lower bound when this is not zero.
    pub unreadable: u64,
}

/// How a walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    Done(FolderSize),
    Cancelled,
}

/// Walks `root` and sums what it holds. `progress` gets the running sum
/// every `every`, on the calling thread; a walk that ends sooner never
/// calls it.
pub fn folder_size(
    root: &Path,
    cancel: &AtomicBool,
    every: Duration,
    progress: impl FnMut(u64),
) -> Walk {
    folder_size_by(Reader::Bulk, root, cancel, every, progress)
}

/// How folders are read. Only for measuring; the app uses [`folder_size`].
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    /// `read_dir` and `lstat` for each entry.
    Std,
    /// `getattrlistbulk`.
    Bulk,
}

/// [`folder_size`] with the folders read by `reader`.
#[doc(hidden)]
pub fn folder_size_by(
    reader: Reader,
    root: &Path,
    cancel: &AtomicBool,
    every: Duration,
    progress: impl FnMut(u64),
) -> Walk {
    let workers = thread::available_parallelism()
        .map_or(1, NonZero::get)
        .min(MAX_WORKERS);
    match reader {
        Reader::Std => walk(root, cancel, every, workers, || StdLister, progress),
        Reader::Bulk => walk(root, cancel, every, workers, BulkLister::new, progress),
    }
}

/// Whether the walk goes into an entry of this kind and with these flags.
/// A dataless folder is not entered: reading it would download it.
fn enters(kind: Kind, flags: u32) -> bool {
    kind == Kind::Folder && flags & SF_DATALESS == 0
}

fn walk<L: Lister>(
    root: &Path,
    cancel: &AtomicBool,
    every: Duration,
    workers: usize,
    lister: impl Fn() -> L + Sync,
    mut progress: impl FnMut(u64),
) -> Walk {
    let unreadable = FolderSize {
        bytes: 0,
        unreadable: 1,
    };
    let Ok(metadata) = fs::symlink_metadata(root) else {
        return Walk::Done(unreadable);
    };
    if !enters(Kind::of(&metadata), metadata.st_flags()) {
        return Walk::Done(unreadable);
    }
    let device = metadata.dev();
    let shared = Shared::new(root.to_owned());
    thread::scope(|scope| {
        let mut started = 0;
        for _ in 0..workers {
            let spawned = thread::Builder::new()
                .name("orbit-folder-walk".into())
                .spawn_scoped(scope, || work(&shared, lister(), cancel, device));
            match spawned {
                Ok(_) => started += 1,
                Err(err) => {
                    warn!(%err, started, "no more threads to walk the folder");
                    break;
                }
            }
        }
        if started == 0 {
            // No running sum then: this thread is busy walking.
            work(&shared, lister(), cancel, device);
        } else {
            report(&shared, cancel, every, &mut progress);
        }
    });
    // Every walk thread has ended here.
    if cancel.load(Ordering::Relaxed) {
        return Walk::Cancelled;
    }
    Walk::Done(FolderSize {
        bytes: shared.bytes.load(Ordering::Relaxed),
        unreadable: shared.unreadable.load(Ordering::Relaxed),
    })
}

/// The folders waiting to be read. The walk is over when none wait and
/// none are being read.
struct Queue {
    folders: Vec<PathBuf>,
    /// Folders being read right now.
    busy: usize,
    /// Set once the walk is over or cancelled; nothing is taken after.
    stop: bool,
}

/// What the walk threads share.
struct Shared {
    queue: Mutex<Queue>,
    /// Wakes the walk threads: folders were added, or the walk stops.
    work: Condvar,
    /// Wakes the reporting thread: the walk stops.
    over: Condvar,
    bytes: AtomicU64,
    unreadable: AtomicU64,
    /// Files with several hard links already counted. Every folder read
    /// lies on the volume of the root, so the inode alone identifies them.
    linked: Mutex<HashSet<u64>>,
}

impl Shared {
    fn new(root: PathBuf) -> Self {
        Self {
            queue: Mutex::new(Queue {
                folders: vec![root],
                busy: 0,
                stop: false,
            }),
            work: Condvar::new(),
            over: Condvar::new(),
            bytes: AtomicU64::new(0),
            unreadable: AtomicU64::new(0),
            linked: Mutex::new(HashSet::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stop(&self, queue: &mut Queue) {
        queue.stop = true;
        self.work.notify_all();
        self.over.notify_all();
    }

    /// The next folder to read; `None` when the walk is over or cancelled.
    fn take(&self, cancel: &AtomicBool) -> Option<Taken<'_>> {
        let mut queue = self.lock();
        loop {
            if queue.stop {
                return None;
            }
            if cancel.load(Ordering::Relaxed) {
                self.stop(&mut queue);
                return None;
            }
            if let Some(folder) = queue.folders.pop() {
                queue.busy += 1;
                return Some(Taken {
                    shared: self,
                    folder,
                });
            }
            if queue.busy == 0 {
                self.stop(&mut queue);
                return None;
            }
            queue = self
                .work
                .wait(queue)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn add_folders(&self, folders: &mut Vec<PathBuf>) {
        if folders.is_empty() {
            return;
        }
        let mut queue = self.lock();
        queue.folders.append(folders);
        self.work.notify_all();
    }

    fn add_bytes(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        // The closure always returns `Some`, so this never fails.
        let _ = self
            .bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |sum| {
                Some(sum.saturating_add(bytes))
            });
    }

    /// Whether this is the first time the hard-linked file `ino` is seen.
    fn first_link(&self, ino: u64) -> bool {
        self.linked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(ino)
    }
}

/// A folder being read. Dropping it marks it read, also when the thread
/// unwinds, so the others never wait for it forever.
struct Taken<'a> {
    shared: &'a Shared,
    folder: PathBuf,
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        let mut queue = self.shared.lock();
        queue.busy = queue.busy.saturating_sub(1);
        let over = queue.folders.is_empty() && queue.busy == 0;
        if over || thread::panicking() {
            self.shared.stop(&mut queue);
        }
    }
}

/// The body of a walk thread.
fn work<L: Lister>(shared: &Shared, mut lister: L, cancel: &AtomicBool, device: u64) {
    iopolicy::leave_dataless_alone();
    while let Some(taken) = shared.take(cancel) {
        read_folder(shared, &mut lister, &taken.folder, device, cancel);
    }
}

/// What a thread counted in the folder it reads, not yet shared.
#[derive(Default)]
struct Tally {
    bytes: u64,
    unreadable: u64,
    entries: u32,
    folders: Vec<PathBuf>,
}

impl Tally {
    fn add(&mut self, shared: &Shared, folder: &Path, entry: Entry) {
        match entry.kind {
            Kind::Folder if enters(entry.kind, entry.flags) => {
                self.folders.push(folder.join(&entry.name));
            }
            // Dataless: its content is not on the disk.
            Kind::Folder => self.unreadable += 1,
            Kind::File | Kind::Link => {
                let once =
                    entry.kind == Kind::Link || entry.nlink <= 1 || shared.first_link(entry.ino);
                if once {
                    self.bytes = self.bytes.saturating_add(entry.on_disk);
                }
            }
            Kind::Other => {}
        }
    }

    /// Hands what was counted so far to the others: the sum grows while a
    /// big folder is read, and its subfolders can be read meanwhile.
    fn flush(&mut self, shared: &Shared) {
        shared.add_bytes(std::mem::take(&mut self.bytes));
        let unreadable = std::mem::take(&mut self.unreadable);
        if unreadable > 0 {
            shared.unreadable.fetch_add(unreadable, Ordering::Relaxed);
        }
        shared.add_folders(&mut self.folders);
    }
}

fn read_folder<L: Lister>(
    shared: &Shared,
    lister: &mut L,
    folder: &Path,
    device: u64,
    cancel: &AtomicBool,
) {
    let mut tally = Tally::default();
    let listed = lister.list(folder, device, &mut |found| {
        if cancel.load(Ordering::Relaxed) {
            return ControlFlow::Break(());
        }
        match found {
            Found::Entry(entry) => tally.add(shared, folder, entry),
            Found::Unreadable => tally.unreadable += 1,
        }
        tally.entries += 1;
        if tally.entries.is_multiple_of(FLUSH_EVERY) {
            tally.flush(shared);
        }
        ControlFlow::Continue(())
    });
    match listed {
        Ok(Listed::Whole | Listed::Stopped | Listed::OtherVolume) => {}
        Ok(Listed::Broken(err)) | Err(err) => {
            trace!(folder = %folder.display(), %err, "folder not read");
            tally.unreadable += 1;
        }
    }
    tally.flush(shared);
}

/// Calls `progress` with the running sum every `every` until the walk is
/// over. Sees `cancel` at each call too, for walk threads stuck in a read.
fn report(shared: &Shared, cancel: &AtomicBool, every: Duration, progress: &mut impl FnMut(u64)) {
    let mut queue = shared.lock();
    let mut deadline = Instant::now() + every;
    loop {
        if queue.stop {
            return;
        }
        let now = Instant::now();
        if now < deadline {
            queue = match shared.over.wait_timeout(queue, deadline - now) {
                Ok((queue, _)) => queue,
                Err(poisoned) => poisoned.into_inner().0,
            };
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            shared.stop(&mut queue);
            return;
        }
        drop(queue);
        progress(shared.bytes.load(Ordering::Relaxed));
        deadline = Instant::now() + every;
        queue = shared.lock();
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    use super::*;
    use crate::platform::test_dir::TempDir;

    const EVERY: Duration = Duration::from_millis(5);

    /// Every reader with one and with several threads.
    const WAYS: [(Reader, usize); 4] = [
        (Reader::Std, 1),
        (Reader::Std, 4),
        (Reader::Bulk, 1),
        (Reader::Bulk, 4),
    ];

    fn walk_by(
        (reader, workers): (Reader, usize),
        root: &Path,
        cancel: &AtomicBool,
        progress: impl FnMut(u64),
    ) -> Walk {
        match reader {
            Reader::Std => walk(root, cancel, EVERY, workers, || StdLister, progress),
            Reader::Bulk => walk(root, cancel, EVERY, workers, BulkLister::new, progress),
        }
    }

    /// The walk of `root` in every way.
    fn walks(root: &Path) -> Vec<Walk> {
        WAYS.into_iter()
            .map(|way| walk_by(way, root, &AtomicBool::new(false), |_| {}))
            .collect()
    }

    fn done(bytes: u64, unreadable: u64) -> Walk {
        Walk::Done(FolderSize { bytes, unreadable })
    }

    /// The bytes these files take on the disk.
    fn on_disk<P: AsRef<Path>>(paths: impl IntoIterator<Item = P>) -> io::Result<u64> {
        let mut sum = 0;
        for path in paths {
            sum += fs::symlink_metadata(path)?.blocks() * 512;
        }
        Ok(sum)
    }

    fn assert_walks(root: &Path, expected: Walk) {
        for (way, walk) in WAYS.into_iter().zip(walks(root)) {
            assert_eq!(walk, expected, "{way:?}");
        }
    }

    fn file(name: &str, on_disk: u64) -> Found {
        Found::Entry(Entry {
            name: name.into(),
            kind: Kind::File,
            on_disk,
            ino: 0,
            nlink: 1,
            flags: 0,
        })
    }

    fn folder(name: &str) -> Found {
        Found::Entry(Entry {
            name: name.into(),
            kind: Kind::Folder,
            on_disk: 0,
            ino: 0,
            nlink: 1,
            flags: 0,
        })
    }

    /// Hands out the same entries for every folder.
    struct Fake<F: FnMut(&mut dyn FnMut(Found) -> ControlFlow<()>) -> io::Result<Listed>>(F);

    impl<F> Lister for Fake<F>
    where
        F: FnMut(&mut dyn FnMut(Found) -> ControlFlow<()>) -> io::Result<Listed>,
    {
        fn list(
            &mut self,
            _dir: &Path,
            _device: u64,
            each: &mut dyn FnMut(Found) -> ControlFlow<()>,
        ) -> io::Result<Listed> {
            (self.0)(each)
        }
    }

    fn hand_out(
        each: &mut dyn FnMut(Found) -> ControlFlow<()>,
        found: impl IntoIterator<Item = Found>,
    ) -> io::Result<Listed> {
        for found in found {
            if each(found).is_break() {
                return Ok(Listed::Stopped);
            }
        }
        Ok(Listed::Whole)
    }

    #[test]
    fn nested_files_add_up() -> io::Result<()> {
        let dir = TempDir::new();
        fs::write(dir.join("a"), [0; 10])?;
        fs::create_dir_all(dir.join("x/y"))?;
        fs::write(dir.join("x/b"), [0; 200])?;
        fs::write(dir.join("x/y/c"), [0; 3000])?;
        let expected = on_disk([dir.join("a"), dir.join("x/b"), dir.join("x/y/c")])?;
        assert!(expected >= 3210);
        assert_walks(dir.path(), done(expected, 0));
        Ok(())
    }

    #[test]
    fn an_empty_folder_is_zero() {
        let dir = TempDir::new();
        assert_walks(dir.path(), done(0, 0));
    }

    #[test]
    fn a_link_counts_itself_not_what_it_points_to() -> io::Result<()> {
        let outside = TempDir::new();
        let big = outside.join("big");
        fs::write(&big, vec![0; 100_000])?;
        let dir = TempDir::new();
        let link = dir.join("link");
        symlink(&big, &link)?;
        let own = on_disk([&link])?;
        assert!(own < 100_000);
        assert_walks(dir.path(), done(own, 0));
        Ok(())
    }

    #[test]
    fn a_hard_linked_file_counts_once() -> io::Result<()> {
        let dir = TempDir::new();
        fs::write(dir.join("a"), [0; 500])?;
        fs::hard_link(dir.join("a"), dir.join("b"))?;
        let once = on_disk([dir.join("a")])?;
        assert!(once > 0);
        assert_walks(dir.path(), done(once, 0));
        Ok(())
    }

    #[test]
    fn a_sparse_file_counts_the_blocks_it_uses() -> io::Result<()> {
        let dir = TempDir::new();
        let sparse = fs::File::create(dir.join("disk.raw"))?;
        sparse.set_len(10 << 30)?;
        drop(sparse);
        let used = on_disk([dir.join("disk.raw")])?;
        assert!(used < 1 << 20, "{used}");
        assert_walks(dir.path(), done(used, 0));
        Ok(())
    }

    #[test]
    fn a_raised_cancel_stops_the_walk() -> io::Result<()> {
        let dir = TempDir::new();
        fs::write(dir.join("a"), [0; 1])?;
        for way in WAYS {
            let cancel = AtomicBool::new(true);
            assert_eq!(
                walk_by(way, dir.path(), &cancel, |_| {}),
                Walk::Cancelled,
                "{way:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn an_unreadable_folder_is_counted_and_skipped() -> io::Result<()> {
        let dir = TempDir::new();
        fs::write(dir.join("a"), [0; 7])?;
        let closed = dir.join("closed");
        fs::create_dir(&closed)?;
        fs::write(closed.join("hidden"), [0; 100])?;
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000))?;
        // Root reads it anyway; then there is nothing to check.
        let readable = fs::read_dir(&closed).is_ok();
        let results = walks(dir.path());
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755))?;
        let expected = on_disk([dir.join("a")])?;
        if !readable {
            for (way, walk) in WAYS.into_iter().zip(results) {
                assert_eq!(walk, done(expected, 1), "{way:?}");
            }
        }
        Ok(())
    }

    #[test]
    fn a_missing_root_is_unreadable() {
        let dir = TempDir::new();
        assert_walks(&dir.join("missing"), done(0, 1));
    }

    #[test]
    fn a_tree_adds_up_and_a_link_across_folders_counts_once() -> io::Result<()> {
        let dir = TempDir::new();
        let mut expected = 0;
        for folder in 0..30 {
            let path = dir.join(&format!("d{folder}"));
            fs::create_dir(&path)?;
            for index in 0..20 {
                let len = folder * 20 + index + 1;
                let file = path.join(format!("f{index}"));
                fs::write(&file, vec![0; len])?;
                expected += on_disk([file])?;
            }
        }
        fs::hard_link(dir.join("d0/f0"), dir.join("d29/linked"))?;
        assert_walks(dir.path(), done(expected, 0));
        Ok(())
    }

    #[test]
    fn the_running_sum_never_goes_down() -> io::Result<()> {
        let dir = TempDir::new();
        let mut expected = 0;
        for folder in 0..20 {
            let path = dir.join(&format!("d{folder}"));
            fs::create_dir(&path)?;
            for index in 0..300 {
                let file = path.join(format!("f{index}"));
                fs::write(&file, [0; 3])?;
                expected += on_disk([file])?;
            }
        }
        for way in WAYS {
            let mut seen = Vec::new();
            let walk = walk_by(way, dir.path(), &AtomicBool::new(false), |bytes| {
                seen.push(bytes);
            });
            assert_eq!(walk, done(expected, 0), "{way:?}");
            assert!(seen.windows(2).all(|pair| pair[0] <= pair[1]), "{seen:?}");
            assert!(seen.iter().all(|&bytes| bytes <= expected), "{seen:?}");
        }
        Ok(())
    }

    #[test]
    fn the_sum_grows_while_one_big_folder_is_read() {
        let dir = TempDir::new();
        let (sender, receiver) = mpsc::channel();
        let receiver = Mutex::new(receiver);
        let lister = || {
            Fake(|each: &mut dyn FnMut(Found) -> ControlFlow<()>| {
                hand_out(each, (0..300).map(|index| file(&format!("f{index}"), 1)))?;
                // Wait until the running sum shows part of this folder.
                let shown = receiver
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .recv_timeout(Duration::from_secs(5));
                assert!(shown.is_ok(), "no running sum within the folder");
                hand_out(each, (300..400).map(|index| file(&format!("f{index}"), 1)))
            })
        };
        let mut seen = Vec::new();
        let result = walk(
            dir.path(),
            &AtomicBool::new(false),
            EVERY,
            1,
            lister,
            |bytes| {
                seen.push(bytes);
                if bytes > 0 {
                    let _ = sender.send(());
                }
            },
        );
        assert_eq!(result, done(400, 0));
        assert!(
            seen.iter().any(|&bytes| bytes > 0 && bytes < 400),
            "{seen:?}"
        );
    }

    #[test]
    fn a_cancel_ends_every_walk_thread() {
        let dir = TempDir::new();
        let cancel = AtomicBool::new(false);
        let reading = AtomicUsize::new(0);
        let lister = || {
            Fake(|each: &mut dyn FnMut(Found) -> ControlFlow<()>| {
                reading.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_micros(200));
                // A folder inside every folder: the walk never ends alone.
                let listed = hand_out(each, [file("f", 1), folder("d"), folder("e")]);
                reading.fetch_sub(1, Ordering::SeqCst);
                listed
            })
        };
        let result = walk(dir.path(), &cancel, EVERY, 4, lister, |_| {
            cancel.store(true, Ordering::Relaxed);
        });
        assert_eq!(result, Walk::Cancelled);
        assert_eq!(reading.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_sum_saturates() {
        let dir = TempDir::new();
        let half = u64::MAX / 2 + 1;
        let lister = || {
            Fake(|each: &mut dyn FnMut(Found) -> ControlFlow<()>| {
                hand_out(each, [file("a", half), file("b", half)])
            })
        };
        let result = walk(
            dir.path(),
            &AtomicBool::new(false),
            EVERY,
            2,
            lister,
            |_| {},
        );
        assert_eq!(result, done(u64::MAX, 0));
    }

    #[test]
    fn a_bulk_listing_broken_midway_keeps_what_it_counted() -> io::Result<()> {
        use super::bulk::build::Fields;

        let dir = TempDir::new();
        // Read again by mistake, this file would show in the sum.
        fs::write(dir.join("real"), [0; 1000])?;
        let mut batch = Fields::file(b"p", 1, 10).record();
        batch.extend(
            Fields {
                error: Some(libc::EACCES as u32),
                ..Fields::file(b"q", 2, 0)
            }
            .record(),
        );
        batch.extend(Fields::file(b"r", 3, 20).record());
        let lister = || {
            let batch = batch.clone();
            let mut calls = 0;
            BulkLister::with_call(move |_, buf: &mut [u8]| {
                calls += 1;
                if calls > 1 {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                buf[..batch.len()].copy_from_slice(&batch);
                Ok(3)
            })
        };
        let result = walk(
            dir.path(),
            &AtomicBool::new(false),
            EVERY,
            1,
            lister,
            |_| {},
        );
        // The record with an error and the broken folder.
        assert_eq!(result, done(30, 2));
        Ok(())
    }

    #[test]
    fn enters_only_folders_on_the_disk() {
        assert!(enters(Kind::Folder, 0));
        assert!(enters(Kind::Folder, libc::UF_HIDDEN));
        assert!(!enters(Kind::Folder, SF_DATALESS));
        assert!(!enters(Kind::File, 0));
        assert!(!enters(Kind::Link, 0));
    }

    /// A disk image mounted at a folder, detached when dropped.
    struct Mounted(PathBuf);

    impl Mounted {
        /// `None` when `hdiutil` could not make or mount the image.
        fn new(image: &Path, point: &Path) -> Option<Self> {
            let made = Command::new("hdiutil")
                .args(["create", "-quiet", "-size", "4m", "-fs", "HFS+"])
                .args(["-volname", "OrbitTest"])
                .arg(image)
                .status();
            if !made.is_ok_and(|status| status.success()) {
                return None;
            }
            let attached = Command::new("hdiutil")
                .args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
                .arg(point)
                .arg(image)
                .status();
            attached
                .is_ok_and(|status| status.success())
                .then(|| Self(point.to_owned()))
        }
    }

    impl Drop for Mounted {
        fn drop(&mut self) {
            let _ = Command::new("hdiutil")
                .args(["detach", "-quiet", "-force"])
                .arg(&self.0)
                .status();
        }
    }

    #[test]
    fn another_volume_is_not_entered() -> io::Result<()> {
        let images = TempDir::new();
        let dir = TempDir::new();
        fs::write(dir.join("a"), [0; 5])?;
        let point = dir.join("volume");
        fs::create_dir(&point)?;
        let Some(_mounted) = Mounted::new(&images.join("volume.dmg"), &point) else {
            eprintln!("skipped: hdiutil could not mount a disk image");
            return Ok(());
        };
        fs::write(point.join("inside"), [0; 1000])?;
        assert_walks(dir.path(), done(on_disk([dir.join("a")])?, 0));
        let device = fs::symlink_metadata(dir.path())?.dev();
        let mut each = |_: Found| ControlFlow::Continue(());
        assert!(matches!(
            StdLister.list(&point, device, &mut each)?,
            Listed::OtherVolume
        ));
        assert!(matches!(
            BulkLister::new().list(&point, device, &mut each)?,
            Listed::OtherVolume
        ));
        Ok(())
    }
}
