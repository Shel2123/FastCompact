//! The ZIP job: measure, write, publish.

use std::fs::File;
use std::io::{self, BufWriter, ErrorKind, Read, Write};
use std::path::Path;

use orbit_app::messages::{Fraction, JobError, JobOutput, JobSpec, Progress};
use orbit_app::ports::JobContext;
use tempfile::NamedTempFile;
use tracing::info;
use zip::ZipWriter;
use zip::result::ZipError;
use zip::write::FullFileOptions;

use super::ArchiveOptions;
use super::entry::{options, sidecar_name, sidecar_options};
use super::tree::{self, Entry, Kind, Root, Walk};
use crate::publish::{self, folder, io_error};

/// Bytes read from a source file between two checks for cancellation.
const BLOCK: usize = 256 << 10;

/// Size of the buffer in front of the temporary file.
const OUTPUT_BUFFER: usize = 256 << 10;

/// Runs the ZIP job of `spec`, which [`super::check`] accepted.
///
/// `hook` is called with the path of every entry of the writing pass
/// between its check and its reading; only tests pass something.
pub(super) fn archive(
    spec: &JobSpec,
    context: &JobContext,
    options: ArchiveOptions<'_>,
    hook: &mut dyn FnMut(&str),
) -> Result<JobOutput, JobError> {
    let target = &spec.target;
    let Some(dir) = target.path.parent() else {
        return Err(JobError::Internal("the target has no folder".into()));
    };
    context.report(Progress::Indeterminate);
    let root = tree::open_root(target)?;
    let total = measure(&root, context)?;
    let temp = publish::create_temp(dir)?;
    let mut writer = Writer {
        zip: ZipWriter::new(BufWriter::with_capacity(OUTPUT_BUFFER, temp)),
        options,
        context,
        dir,
        total,
        done: 0,
        buffer: vec![0; BLOCK],
        counts: Counts::default(),
    };
    match root.meta.kind {
        Kind::File => writer.root_file(root, target, hook)?,
        Kind::Directory => writer.root_folder(root, hook)?,
        Kind::Symlink | Kind::Special => {
            return Err(JobError::Internal(
                "the root is neither a file nor a folder".into(),
            ));
        }
    }
    let Writer {
        zip, counts, dir, ..
    } = writer;
    let output = zip.finish().map_err(|error| zip_error(error, dir))?;
    let path = publish::commit(output, dir, &spec.destination, || context.is_cancelled())?;
    info!(
        files = counts.files,
        folders = counts.folders,
        links = counts.links,
        sidecars = counts.sidecars,
        skipped_special = counts.skipped_special,
        skipped_other_volume = counts.skipped_other_volume,
        bytes = total,
        "zip archive created"
    );
    Ok(JobOutput { path })
}

/// Sum of the sizes of the regular files, for the fraction of progress.
fn measure(root: &Root, context: &JobContext) -> Result<u64, JobError> {
    if root.meta.kind != Kind::Directory {
        return Ok(root.meta.size);
    }
    let object = root
        .object
        .try_clone()
        .map_err(|error| io_error(&error, &format!("couldn't read “{}”", root.name)))?;
    let mut no_hook = |_: &str| {};
    let mut walk = Walk::new(object, root.name.clone(), &root.meta, &mut no_hook)?;
    let mut total = 0_u64;
    while let Some(entry) = walk.next()? {
        if context.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        if let Entry::File(at) = entry {
            total = total.saturating_add(at.meta.size);
        }
    }
    Ok(total)
}

#[derive(Debug, Default)]
struct Counts {
    files: u64,
    folders: u64,
    links: u64,
    sidecars: u64,
    skipped_special: u64,
    skipped_other_volume: u64,
}

struct Writer<'a, 'o> {
    zip: ZipWriter<BufWriter<NamedTempFile>>,
    options: ArchiveOptions<'o>,
    context: &'a JobContext,
    /// Folder of the target, where the archive goes.
    dir: &'a Path,
    /// Bytes the measuring pass found.
    total: u64,
    /// Source bytes read so far.
    done: u64,
    buffer: Vec<u8>,
    counts: Counts,
}

impl Writer<'_, '_> {
    /// A chosen file: changes made while it was read fail the job.
    fn root_file(
        &mut self,
        root: Root,
        target: &orbit_domain::target::FileTarget,
        hook: &mut dyn FnMut(&str),
    ) -> Result<(), JobError> {
        let Root {
            name,
            mut object,
            meta,
        } = root;
        let options = options(&meta, self.options.local_time)?;
        hook(&name);
        self.file(&name, &mut object, &options)?;
        tree::check_root(target, &object, &name)?;
        self.sidecar(&name, &object, &options)
    }

    /// A chosen folder and everything below it.
    fn root_folder(&mut self, root: Root, hook: &mut dyn FnMut(&str)) -> Result<(), JobError> {
        let Root { name, object, meta } = root;
        self.folder(&name, &object, &meta)?;
        let mut walk = Walk::new(object, name, &meta, hook)?;
        while let Some(entry) = walk.next()? {
            self.check_cancelled()?;
            match entry {
                Entry::File(at) => {
                    let mut file = tree::open_file(&at)?;
                    let options = options(&at.meta, self.options.local_time)?;
                    self.file(&at.path, &mut file, &options)?;
                    self.sidecar(&at.path, &file, &options)?;
                }
                Entry::Directory { path, meta, dir } => self.folder(&path, dir, &meta)?,
                Entry::Symlink(at) => {
                    let link = tree::read_link(&at)?;
                    let options = options(&at.meta, self.options.local_time)?;
                    self.zip
                        .add_symlink(at.path.as_str(), link, options)
                        .map_err(|error| zip_error(error, self.dir))?;
                    self.counts.links += 1;
                }
            }
        }
        self.counts.skipped_special = walk.skipped_special;
        self.counts.skipped_other_volume = walk.skipped_other_volume;
        Ok(())
    }

    fn folder(&mut self, path: &str, dir: &File, meta: &tree::Meta) -> Result<(), JobError> {
        let options = options(meta, self.options.local_time)?;
        self.zip
            .add_directory(path, options.clone())
            .map_err(|error| zip_error(error, self.dir))?;
        self.counts.folders += 1;
        self.sidecar(path, dir, &options)
    }

    /// Streams an open file into a new entry, checking for cancellation
    /// before every block.
    fn file(
        &mut self,
        path: &str,
        file: &mut File,
        options: &FullFileOptions<'static, 'static>,
    ) -> Result<(), JobError> {
        self.zip
            .start_file(path, options.clone())
            .map_err(|error| zip_error(error, self.dir))?;
        loop {
            self.check_cancelled()?;
            let read = match file.read(&mut self.buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(io_error(&error, &format!("couldn't read “{path}”")));
                }
            };
            let block = self
                .buffer
                .get(..read)
                .ok_or_else(|| JobError::Internal("a read went past its buffer".into()))?;
            self.zip
                .write_all(block)
                .map_err(|error| write_error(&error, self.dir))?;
            self.advance(read);
        }
        self.counts.files += 1;
        Ok(())
    }

    /// Stores what the platform adds to an object, right after its entry.
    fn sidecar(
        &mut self,
        path: &str,
        object: &File,
        options: &FullFileOptions<'static, 'static>,
    ) -> Result<(), JobError> {
        let Some(pack) = self.options.sidecar.as_mut() else {
            return Ok(());
        };
        let bytes = pack(object).map_err(|error| {
            io_error(&error, &format!("couldn't read the metadata of “{path}”"))
        })?;
        let Some(bytes) = bytes else {
            return Ok(());
        };
        self.zip
            .start_file(sidecar_name(path), sidecar_options(options))
            .map_err(|error| zip_error(error, self.dir))?;
        self.zip
            .write_all(&bytes)
            .map_err(|error| write_error(&error, self.dir))?;
        self.counts.sidecars += 1;
        Ok(())
    }

    fn advance(&mut self, read: usize) {
        self.done = self
            .done
            .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if self.total == 0 {
            return;
        }
        // Files that grew while they were read can push it past 1.
        let fraction = (self.done as f64 / self.total as f64).min(1.0);
        if let Some(fraction) = Fraction::new(fraction) {
            self.context.report(Progress::Fraction(fraction));
        }
    }

    fn check_cancelled(&self) -> Result<(), JobError> {
        if self.context.is_cancelled() {
            Err(JobError::Cancelled)
        } else {
            Ok(())
        }
    }
}

fn write_error(error: &io::Error, dir: &Path) -> JobError {
    io_error(
        error,
        &format!("couldn't write the archive in {}", folder(dir)),
    )
}

fn zip_error(error: ZipError, dir: &Path) -> JobError {
    match error {
        ZipError::Io(error) => write_error(&error, dir),
        other => JobError::Io {
            kind: ErrorKind::Other,
            message: format!("couldn't write the archive in {}: {other}", folder(dir)),
        },
    }
}
