use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orbit_app::messages::{JobError, JobId, JobOutput, JobSpec, Progress};
use orbit_app::ports::{CancelToken, JobContext, OperationExecutor};
use orbit_domain::operation::{Destination, FileName, FileNamePolicy, OperationSpec};
use orbit_domain::target::{
    FileFacts, FileIdentity, FileKind, FileTarget, FileVersion, SourceChanged,
};
use tempfile::TempDir;
use zip::ZipArchive;
use zip::extra_fields::ExtraField;

use super::entry::dos_time;
use super::{ArchiveOptions, LocalTime, Sidecar, archive_zip, check, writer};
use crate::executor::PortableExecutor;
use crate::publish::TEMP_PREFIX;

// --- helpers ---------------------------------------------------------------

fn tempdir() -> TempDir {
    tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"))
}

fn ok<T>(result: io::Result<T>, what: &str) -> T {
    result.unwrap_or_else(|error| panic!("{what}: {error}"))
}

fn write(path: &Path, content: &[u8]) {
    ok(fs::write(path, content), &path.display().to_string());
}

fn mkdir(path: &Path) {
    ok(fs::create_dir(path), &path.display().to_string());
}

fn set_mode(path: &Path, mode: u32) {
    ok(
        fs::set_permissions(path, Permissions::from_mode(mode)),
        "chmod",
    );
}

fn set_modified(path: &Path, seconds: u64) {
    let file = ok(File::open(path), "open for touch");
    ok(
        file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
        "touch",
    );
}

fn mkfifo(path: &Path) {
    let status = ok(Command::new("mkfifo").arg(path).status(), "mkfifo");
    assert!(status.success(), "mkfifo failed");
}

/// The target as the macOS resolver reads it, without the package check.
fn target(path: &Path) -> FileTarget {
    let metadata = ok(fs::symlink_metadata(path), "stat target");
    let kind = if metadata.file_type().is_symlink() {
        FileKind::Symlink
    } else if metadata.is_dir() {
        FileKind::Directory
    } else {
        FileKind::File
    };
    FileTarget {
        path: path.to_owned(),
        identity: FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        version: FileVersion {
            size: metadata.len(),
            modified: ok(metadata.modified(), "mtime"),
        },
        facts: FileFacts { kind, image: None },
    }
}

fn spec(target: FileTarget) -> JobSpec {
    let name = target.path.file_name().unwrap_or(OsStr::new("")).to_owned();
    JobSpec {
        id: JobId::new(1),
        destination: OperationSpec::ArchiveZip.destination(&name),
        operation: OperationSpec::ArchiveZip,
        target,
    }
}

/// A context with a cancel token and a record of the reported progress.
struct Recorder {
    cancel: CancelToken,
    seen: Arc<Mutex<Vec<Progress>>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            cancel: CancelToken::new(),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A context that cancels the job at the first fraction it reports when
    /// `cancel_on_fraction` is set.
    fn context(&self, cancel_on_fraction: bool) -> JobContext {
        let seen = Arc::clone(&self.seen);
        let cancel = self.cancel.clone();
        JobContext::new(self.cancel.clone(), move |progress| {
            if let Ok(mut seen) = seen.lock() {
                seen.push(progress);
            }
            if cancel_on_fraction && matches!(progress, Progress::Fraction(_)) {
                cancel.cancel();
            }
        })
    }

    fn seen(&self) -> Vec<Progress> {
        self.seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }
}

fn options(sidecar: Option<Sidecar<'_>>) -> ArchiveOptions<'_> {
    ArchiveOptions {
        local_time: LocalTime::utc,
        sidecar,
    }
}

fn run(path: &Path) -> Result<JobOutput, JobError> {
    archive_zip(
        &spec(target(path)),
        &Recorder::new().context(false),
        options(None),
    )
}

/// Runs the job with a hook between the check and the reading of every
/// entry of the writing pass.
fn run_with_hook(path: &Path, hook: &mut dyn FnMut(&str)) -> Result<JobOutput, JobError> {
    let spec = spec(target(path));
    check(&spec)?;
    writer::archive(&spec, &Recorder::new().context(false), options(None), hook)
}

fn published(output: &Result<JobOutput, JobError>) -> PathBuf {
    match output {
        Ok(output) => output.path.clone(),
        Err(error) => panic!("the job failed: {error:?}"),
    }
}

/// Names in `dir` that start with the temporary prefix.
fn leftovers(dir: &Path) -> Vec<String> {
    ok(fs::read_dir(dir), "list")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(TEMP_PREFIX))
        .collect()
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = ok(fs::read_dir(dir), "list")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[derive(Debug)]
struct Item {
    name: String,
    directory: bool,
    symlink: bool,
    mode: Option<u32>,
    data: Vec<u8>,
    dos: Option<zip::DateTime>,
    unix_time: Option<u32>,
}

fn read_archive(path: &Path) -> Vec<Item> {
    let mut archive =
        ZipArchive::new(ok(File::open(path), "open zip")).unwrap_or_else(|e| panic!("zip: {e}"));
    (0..archive.len())
        .map(|index| {
            let mut file = archive
                .by_index(index)
                .unwrap_or_else(|e| panic!("entry {index}: {e}"));
            let unix_time = file.extra_data_fields().find_map(|field| match field {
                ExtraField::ExtendedTimestamp(stamp) => stamp.mod_time(),
                _ => None,
            });
            let mut data = Vec::new();
            ok(file.read_to_end(&mut data), "read entry");
            Item {
                name: file
                    .name()
                    .unwrap_or_else(|e| panic!("name: {e}"))
                    .into_owned(),
                directory: file.is_dir(),
                symlink: file.is_symlink(),
                mode: file.unix_mode(),
                data,
                dos: file.last_modified(),
                unix_time,
            }
        })
        .collect()
}

fn names(items: &[Item]) -> Vec<&str> {
    items.iter().map(|item| item.name.as_str()).collect()
}

fn item<'i>(items: &'i [Item], name: &str) -> &'i Item {
    items
        .iter()
        .find(|item| item.name == name)
        .unwrap_or_else(|| panic!("no entry {name} in {:?}", names(items)))
}

fn expect_error(result: Result<JobOutput, JobError>, expected: &JobError) {
    match result {
        Ok(output) => panic!("expected {expected:?}, got {output:?}"),
        Err(error) => assert_eq!(&error, expected),
    }
}

/// A folder `Docs` with a file, for the tests of swaps and failures.
fn docs(dir: &Path) -> PathBuf {
    let docs = dir.join("Docs");
    mkdir(&docs);
    write(&docs.join("a.txt"), b"alpha");
    mkdir(&docs.join("sub"));
    write(&docs.join("sub").join("b.txt"), b"beta");
    docs
}

/// Nothing was published and nothing temporary is left in `dir`.
fn nothing_left(dir: &Path) {
    assert!(leftovers(dir).is_empty(), "{:?}", leftovers(dir));
    assert!(!dir.join("Docs.zip").exists());
}

// --- contents --------------------------------------------------------------

#[test]
fn a_file_becomes_one_entry_with_its_metadata() {
    let dir = tempdir();
    let file = dir.path().join("a.txt");
    write(&file, b"hello, archive");
    set_mode(&file, 0o640);
    set_modified(&file, 1_700_000_001);

    let output = run(&file);
    let zip = published(&output);
    assert_eq!(zip, dir.path().join("a.txt.zip"));
    let items = read_archive(&zip);
    assert_eq!(names(&items), ["a.txt"]);
    let entry = item(&items, "a.txt");
    assert_eq!(entry.data, b"hello, archive");
    assert_eq!(entry.mode.map(|mode| mode & 0o7777), Some(0o640));
    assert_eq!(entry.unix_time, Some(1_700_000_001));
    let modified = UNIX_EPOCH + Duration::from_secs(1_700_000_001);
    assert_eq!(entry.dos, Some(dos_time(LocalTime::utc(modified))));
    assert!(leftovers(dir.path()).is_empty());
}

#[test]
fn a_folder_keeps_its_structure() {
    let dir = tempdir();
    let docs = dir.path().join("Docs");
    mkdir(&docs);
    mkdir(&docs.join("empty"));
    mkdir(&docs.join("sub"));
    write(&docs.join("sub").join("b.txt"), b"beta");
    symlink("sub/b.txt", docs.join("link")).unwrap_or_else(|e| panic!("symlink: {e}"));
    write(&docs.join("run.sh"), b"#!/bin/sh\n");
    set_mode(&docs.join("run.sh"), 0o755);
    write(&docs.join(".DS_Store"), b"finder");
    // "Привет" with "и" + combining breve instead of "й" (NFD).
    let nfd = "Прив\u{0438}\u{0306}ет";
    write(&docs.join(nfd), b"nfd");
    set_modified(&docs.join("sub"), 1_600_000_000);

    let zip = published(&run(&docs));
    let items = read_archive(&zip);
    let nfd_entry = format!("Docs/{nfd}");
    assert_eq!(
        names(&items),
        [
            "Docs/",
            "Docs/.DS_Store",
            "Docs/empty/",
            "Docs/link",
            "Docs/run.sh",
            "Docs/sub/",
            "Docs/sub/b.txt",
            nfd_entry.as_str(),
        ]
    );
    assert!(item(&items, "Docs/").directory);
    assert!(item(&items, "Docs/empty/").directory);
    let link = item(&items, "Docs/link");
    assert!(link.symlink);
    assert_eq!(link.data, b"sub/b.txt");
    let script = item(&items, "Docs/run.sh");
    assert_eq!(script.mode.map(|mode| mode & 0o777), Some(0o755));
    assert_eq!(item(&items, "Docs/sub/b.txt").data, b"beta");
    assert_eq!(item(&items, &nfd_entry).data, b"nfd");
    assert_eq!(item(&items, "Docs/sub/").unix_time, Some(1_600_000_000));
}

#[test]
fn special_files_are_skipped_without_being_opened() {
    let dir = tempdir();
    let docs = docs(dir.path());
    // Opening a FIFO for reading blocks until a writer comes: the test
    // would hang if the walk opened it.
    mkfifo(&docs.join("pipe"));

    let items = read_archive(&published(&run(&docs)));
    assert_eq!(
        names(&items),
        ["Docs/", "Docs/a.txt", "Docs/sub/", "Docs/sub/b.txt"]
    );
}

#[test]
fn a_taken_name_gets_the_next_candidate() {
    let dir = tempdir();
    let docs = docs(dir.path());
    write(&dir.path().join("Docs.zip"), b"older");

    let zip = published(&run(&docs));
    assert_eq!(zip, dir.path().join("Docs 2.zip"));
    assert_eq!(ok(fs::read(dir.path().join("Docs.zip")), "read"), b"older");
}

// --- sidecars --------------------------------------------------------------

#[test]
fn sidecars_follow_their_objects() {
    let dir = tempdir();
    let docs = docs(dir.path());
    symlink("a.txt", docs.join("link")).unwrap_or_else(|e| panic!("symlink: {e}"));
    let mut seen = 0;
    let mut pack = |object: &File| -> io::Result<Option<Vec<u8>>> {
        seen += 1;
        let size = object.metadata()?.len();
        Ok(Some(format!("sidecar of {size}").into_bytes()))
    };

    let output = archive_zip(
        &spec(target(&docs)),
        &Recorder::new().context(false),
        options(Some(&mut pack)),
    );
    let items = read_archive(&published(&output));
    assert_eq!(
        names(&items),
        [
            "Docs/",
            "__MACOSX/._Docs",
            "Docs/a.txt",
            "__MACOSX/Docs/._a.txt",
            "Docs/link",
            "Docs/sub/",
            "__MACOSX/Docs/._sub",
            "Docs/sub/b.txt",
            "__MACOSX/Docs/sub/._b.txt",
        ]
    );
    // Links get no sidecar.
    assert_eq!(seen, 4);
    // The sidecar got the descriptor of its own object.
    assert_eq!(item(&items, "__MACOSX/Docs/._a.txt").data, b"sidecar of 5");
}

#[test]
fn a_file_root_gets_its_sidecar_at_the_top() {
    let dir = tempdir();
    let file = dir.path().join("a.txt");
    write(&file, b"alpha");
    let mut pack = |_: &File| -> io::Result<Option<Vec<u8>>> { Ok(Some(b"meta".to_vec())) };

    let output = archive_zip(
        &spec(target(&file)),
        &Recorder::new().context(false),
        options(Some(&mut pack)),
    );
    let items = read_archive(&published(&output));
    assert_eq!(names(&items), ["a.txt", "__MACOSX/._a.txt"]);
}

#[test]
fn no_sidecar_bytes_means_no_entry() {
    let dir = tempdir();
    let docs = docs(dir.path());
    let mut pack = |_: &File| -> io::Result<Option<Vec<u8>>> { Ok(None) };

    let output = archive_zip(
        &spec(target(&docs)),
        &Recorder::new().context(false),
        options(Some(&mut pack)),
    );
    let items = read_archive(&published(&output));
    assert!(items.iter().all(|item| !item.name.starts_with("__MACOSX")));
}

#[test]
fn a_failing_sidecar_fails_the_job() {
    let dir = tempdir();
    let docs = docs(dir.path());
    let mut pack = |object: &File| -> io::Result<Option<Vec<u8>>> {
        if object.metadata()?.is_file() {
            Err(io::Error::from(ErrorKind::PermissionDenied))
        } else {
            Ok(None)
        }
    };

    let output = archive_zip(
        &spec(target(&docs)),
        &Recorder::new().context(false),
        options(Some(&mut pack)),
    );
    match output {
        Err(JobError::Io { kind, message }) => {
            assert_eq!(kind, ErrorKind::PermissionDenied);
            assert!(message.contains("“Docs/a.txt”"), "{message}");
        }
        other => panic!("expected an I/O error, got {other:?}"),
    }
    nothing_left(dir.path());
}

// --- swaps and changes -----------------------------------------------------

/// Runs the job on `Docs` and swaps `entry` with `swap` just before the
/// writing pass opens it.
fn swapped(dir: &Path, entry: &str, swap: impl Fn(&Path)) -> Result<JobOutput, JobError> {
    let docs = docs(dir);
    let mut done = false;
    let mut hook = |path: &str| {
        if path == entry && !done {
            done = true;
            swap(&dir.join(path));
        }
    };
    let result = run_with_hook(&docs, &mut hook);
    assert!(done, "the hook never saw {entry}");
    result
}

/// A file outside the tree that must never be read.
fn secret(dir: &Path) -> PathBuf {
    let outside = dir.join("outside");
    mkdir(&outside);
    write(&outside.join("secret.txt"), b"secret");
    outside
}

#[test]
fn a_file_swapped_for_an_outward_link_is_not_followed() {
    let dir = tempdir();
    let outside = secret(dir.path());
    let result = swapped(dir.path(), "Docs/a.txt", |path| {
        ok(fs::remove_file(path), "remove");
        symlink(outside.join("secret.txt"), path).unwrap_or_else(|e| panic!("symlink: {e}"));
    });
    expect_error(result, &SourceChanged::Replaced.into());
    nothing_left(dir.path());
}

#[test]
fn a_file_swapped_for_a_fifo_does_not_block() {
    let dir = tempdir();
    let result = swapped(dir.path(), "Docs/a.txt", |path| {
        ok(fs::remove_file(path), "remove");
        mkfifo(path);
    });
    expect_error(result, &SourceChanged::Replaced.into());
    nothing_left(dir.path());
}

#[test]
fn a_file_swapped_for_another_file_is_refused() {
    let dir = tempdir();
    let result = swapped(dir.path(), "Docs/a.txt", |path| {
        ok(fs::remove_file(path), "remove");
        write(path, b"other");
    });
    expect_error(result, &SourceChanged::Replaced.into());
    nothing_left(dir.path());
}

#[test]
fn a_folder_swapped_for_an_outward_link_is_not_followed() {
    let dir = tempdir();
    let outside = secret(dir.path());
    let result = swapped(dir.path(), "Docs/sub", |path| {
        ok(fs::rename(path, path.with_file_name("moved")), "move");
        symlink(&outside, path).unwrap_or_else(|e| panic!("symlink: {e}"));
    });
    expect_error(result, &SourceChanged::Replaced.into());
    nothing_left(dir.path());
}

#[test]
fn a_link_swapped_for_a_file_is_refused() {
    let dir = tempdir();
    let docs = docs(dir.path());
    symlink("a.txt", docs.join("link")).unwrap_or_else(|e| panic!("symlink: {e}"));
    let mut hook = |path: &str| {
        if path == "Docs/link" {
            ok(fs::remove_file(dir.path().join(path)), "remove");
            write(&dir.path().join(path), b"file now");
        }
    };
    expect_error(
        run_with_hook(&docs, &mut hook),
        &SourceChanged::Replaced.into(),
    );
    nothing_left(dir.path());
}

#[test]
fn a_root_replaced_before_the_start_is_refused() {
    let dir = tempdir();
    let docs = docs(dir.path());
    let spec = spec(target(&docs));
    ok(fs::rename(&docs, dir.path().join("Old")), "move");
    mkdir(&docs);

    let result = archive_zip(&spec, &Recorder::new().context(false), options(None));
    expect_error(result, &SourceChanged::Replaced.into());
    nothing_left(dir.path());
}

#[test]
fn a_root_removed_before_the_start_counts_as_replaced() {
    let dir = tempdir();
    let file = dir.path().join("a.txt");
    write(&file, b"alpha");
    let spec = spec(target(&file));
    ok(fs::remove_file(&file), "remove");

    let result = archive_zip(&spec, &Recorder::new().context(false), options(None));
    expect_error(result, &SourceChanged::Replaced.into());
    assert!(leftovers(dir.path()).is_empty());
}

#[test]
fn a_root_swapped_for_a_link_is_refused() {
    let dir = tempdir();
    let file = dir.path().join("a.txt");
    write(&file, b"alpha");
    let outside = secret(dir.path());
    let spec = spec(target(&file));
    ok(fs::remove_file(&file), "remove");
    symlink(outside.join("secret.txt"), &file).unwrap_or_else(|e| panic!("symlink: {e}"));

    let result = archive_zip(&spec, &Recorder::new().context(false), options(None));
    expect_error(result, &SourceChanged::Replaced.into());
    assert!(leftovers(dir.path()).is_empty());
}

#[test]
fn a_chosen_file_that_grows_while_read_is_refused() {
    let dir = tempdir();
    let file = dir.path().join("a.txt");
    write(&file, b"alpha");
    let mut hook = |_: &str| {
        let mut appended = ok(OpenOptions::new().append(true).open(&file), "append");
        ok(appended.write_all(b" and more"), "append");
    };

    expect_error(
        run_with_hook(&file, &mut hook),
        &SourceChanged::MetadataChanged.into(),
    );
    assert!(leftovers(dir.path()).is_empty());
    assert!(!dir.path().join("a.txt.zip").exists());
}

#[test]
fn a_chosen_file_modified_before_the_start_is_refused() {
    let dir = tempdir();
    let file = dir.path().join("a.txt");
    write(&file, b"alpha");
    let spec = spec(target(&file));
    write(&file, b"changed");

    let result = archive_zip(&spec, &Recorder::new().context(false), options(None));
    expect_error(result, &SourceChanged::MetadataChanged.into());
}

// --- failures and cancellation ---------------------------------------------

#[test]
fn an_unreadable_file_fails_with_its_path() {
    let dir = tempdir();
    let docs = docs(dir.path());
    let locked = docs.join("a.txt");
    set_mode(&locked, 0o000);
    if File::open(&locked).is_ok() {
        // Root reads everything; nothing to test.
        set_mode(&locked, 0o644);
        return;
    }

    let result = run(&docs);
    set_mode(&locked, 0o644);
    match result {
        Err(JobError::Io { kind, message }) => {
            assert_eq!(kind, ErrorKind::PermissionDenied);
            assert_eq!(message, "couldn't read “Docs/a.txt”: Permission denied");
        }
        other => panic!("expected an I/O error, got {other:?}"),
    }
    nothing_left(dir.path());
}

#[test]
fn cancelling_while_writing_leaves_nothing() {
    let dir = tempdir();
    let docs = docs(dir.path());
    // Several blocks, so the job reads again after the first fraction.
    write(&docs.join("big.bin"), &vec![7; 1 << 20]);
    let recorder = Recorder::new();

    let result = archive_zip(&spec(target(&docs)), &recorder.context(true), options(None));
    expect_error(result, &JobError::Cancelled);
    nothing_left(dir.path());
}

#[test]
fn cancelling_before_the_start_leaves_nothing() {
    let dir = tempdir();
    let docs = docs(dir.path());
    let recorder = Recorder::new();
    recorder.cancel.cancel();

    let result = archive_zip(
        &spec(target(&docs)),
        &recorder.context(false),
        options(None),
    );
    expect_error(result, &JobError::Cancelled);
    nothing_left(dir.path());
}

#[test]
fn progress_starts_unknown_and_only_grows() {
    let dir = tempdir();
    let docs = docs(dir.path());
    write(&docs.join("big.bin"), &vec![1; 600 << 10]);
    let recorder = Recorder::new();

    published(&archive_zip(
        &spec(target(&docs)),
        &recorder.context(false),
        options(None),
    ));
    let seen = recorder.seen();
    assert_eq!(seen.first(), Some(&Progress::Indeterminate));
    let fractions: Vec<f64> = seen
        .iter()
        .filter_map(|progress| match progress {
            Progress::Fraction(fraction) => Some(fraction.get()),
            Progress::Indeterminate => None,
        })
        .collect();
    assert!(fractions.len() >= 3, "{fractions:?}");
    assert!(
        fractions.windows(2).all(|pair| pair[0] <= pair[1]),
        "{fractions:?}"
    );
    assert_eq!(fractions.last(), Some(&1.0));
}

// --- refusals --------------------------------------------------------------

#[test]
fn only_zip_jobs_for_files_and_folders_are_accepted() {
    let dir = tempdir();
    let file = dir.path().join("a.txt");
    write(&file, b"alpha");
    symlink("a.txt", dir.path().join("link")).unwrap_or_else(|e| panic!("symlink: {e}"));
    let internal = |result: Result<JobOutput, JobError>| {
        assert!(matches!(result, Err(JobError::Internal(_))), "{result:?}");
    };
    let context = Recorder::new().context(false);

    let mut convert = spec(target(&file));
    convert.operation = OperationSpec::ConvertImage {
        format: orbit_domain::operation::OutputFormat::Png,
    };
    internal(archive_zip(&convert, &context, options(None)));

    internal(archive_zip(
        &spec(target(&dir.path().join("link"))),
        &context,
        options(None),
    ));

    let mut exact = spec(target(&file));
    let name =
        FileName::new("a.zip", &FileNamePolicy::UNIVERSAL).unwrap_or_else(|e| panic!("name: {e}"));
    exact.destination = Destination::Exact(name);
    internal(archive_zip(&exact, &context, options(None)));

    assert_eq!(names_in(dir.path()), ["a.txt", "link"]);
}

#[test]
fn the_portable_executor_runs_zip_and_refuses_the_rest() {
    let dir = tempdir();
    let file = dir.path().join("a.txt");
    write(&file, b"alpha");
    let executor = PortableExecutor::new(LocalTime::utc);
    let context = Recorder::new().context(false);

    let zip = published(&executor.execute(&spec(target(&file)), &context));
    assert_eq!(zip, dir.path().join("a.txt.zip"));

    let mut convert = spec(target(&file));
    convert.operation = OperationSpec::ConvertImage {
        format: orbit_domain::operation::OutputFormat::Png,
    };
    assert!(matches!(
        executor.execute(&convert, &context),
        Err(JobError::Internal(_))
    ));
    let symlink_target = dir.path().join("link");
    symlink("a.txt", &symlink_target).unwrap_or_else(|e| panic!("symlink: {e}"));
    assert!(matches!(
        executor.execute(&spec(target(&symlink_target)), &context),
        Err(JobError::Internal(_))
    ));
}

// --- ZIP64 (slow; run by hand) ---------------------------------------------

/// `cargo test -p orbit-ops -- --ignored zip64_many_entries`
#[test]
#[ignore = "writes 70 000 files"]
fn zip64_many_entries() {
    let dir = tempdir();
    let docs = dir.path().join("Docs");
    mkdir(&docs);
    for index in 0..70_000 {
        write(&docs.join(format!("f{index:05}")), b"x");
    }
    let mut archive = ZipArchive::new(ok(File::open(published(&run(&docs))), "open"))
        .unwrap_or_else(|e| panic!("zip: {e}"));
    assert_eq!(archive.len(), 70_001);
    let mut last = archive
        .by_name("Docs/f69999")
        .unwrap_or_else(|e| panic!("entry: {e}"));
    let mut data = Vec::new();
    ok(last.read_to_end(&mut data), "read");
    assert_eq!(data, b"x");
}

/// `cargo test -p orbit-ops --release -- --ignored zip64_large_offsets`
#[test]
#[ignore = "writes 5 GiB of random data"]
fn zip64_large_offsets() {
    let dir = tempdir();
    let docs = dir.path().join("Docs");
    mkdir(&docs);
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut block = vec![0_u8; 1 << 20];
    for index in 0..5 {
        let mut file = ok(File::create(docs.join(format!("r{index}"))), "create");
        for _ in 0..1024 {
            for chunk in block.chunks_mut(8) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
            }
            ok(file.write_all(&block), "write");
        }
    }
    let zip = published(&run(&docs));
    assert!(ok(fs::metadata(&zip), "stat").len() > 5 << 30);
    let mut archive =
        ZipArchive::new(ok(File::open(&zip), "open")).unwrap_or_else(|e| panic!("zip: {e}"));
    let mut last = archive
        .by_name("Docs/r4")
        .unwrap_or_else(|e| panic!("entry: {e}"));
    let read = ok(io::copy(&mut last, &mut io::sink()), "read");
    assert_eq!(read, 1 << 30);
}

#[test]
fn utc_is_a_valid_local_time_function() {
    // The executor and the tests use it; it must agree with the DOS field
    // of a known time.
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let dos = dos_time(LocalTime::utc(time));
    assert_eq!(
        dos,
        zip::DateTime::from_date_and_time(2023, 11, 14, 22, 13, 20)
            .unwrap_or_else(|_| panic!("range"))
    );
}
