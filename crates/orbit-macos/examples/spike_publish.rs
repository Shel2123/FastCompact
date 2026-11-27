//! Stage 7 spike: checks the assumptions of the publication and ZIP plan
//! (table P1–P10 in `docs/stage-7.md`). Not product code: the ZIP writer
//! here walks by path, which the product must not do.
//!
//! ```text
//! cargo run -p orbit-macos --release --example spike_publish -- <command>
//!   publish <dir>          P1, P3, P4: no-clobber publication in <dir>
//!   sync <dir> <MiB>       P9: sync_all of a <MiB> temporary file in <dir>
//!   zip <src> <out> [lvl]  P5–P8: archive <src> as <out>, deflate level <lvl>
//!   random <dir> <n> <MiB> P8b: <n> files of <MiB> random bytes in <dir>
//!   fill <dir> <count>     P8c: <count> empty files in <dir>
//! ```

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    spike::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod spike {
    use std::error::Error;
    use std::ffi::{CStr, CString, OsStr};
    use std::fs::{self, File, Permissions};
    use std::io::{self, BufWriter, Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::Path;
    use std::process::ExitCode;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use tempfile::{Builder, NamedTempFile};
    use zip::write::FullFileOptions;
    use zip::{CompressionMethod, DateTime, ZipWriter};

    type Result<T> = std::result::Result<T, Box<dyn Error>>;

    pub fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = match args.as_slice() {
            ["publish", dir] => publish(Path::new(dir)),
            ["sync", dir, mib] => mib.parse().map_err(Into::into).and_then(|mib| sync(Path::new(dir), mib)),
            ["zip", src, out] => archive(Path::new(src), Path::new(out), Opts::default()),
            ["zip-pack", src, out] => archive(
                Path::new(src),
                Path::new(out),
                Opts {
                    pack: true,
                    ..Opts::default()
                },
            ),
            ["zip", src, out, level] => level
                .parse()
                .map_err(Into::into)
                .and_then(|level| {
                    let opts = Opts {
                        level: Some(level),
                        pack: false,
                    };
                    archive(Path::new(src), Path::new(out), opts)
                }),
            ["random", dir, count, mib] => count
                .parse()
                .map_err(Into::into)
                .and_then(|count| mib.parse().map_err(Into::into).map(|mib| (count, mib)))
                .and_then(|(count, mib)| random(Path::new(dir), count, mib)),
            ["fill", dir, count] => count.parse().map_err(Into::into).and_then(|count| fill(Path::new(dir), count)),
            _ => Err("usage: publish <dir> | sync <dir> <MiB> | zip <src> <out> [level] | random <dir> <n> <MiB> | fill <dir> <count>".into()),
        };
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        }
    }

    // --- P1, P3, P4 ------------------------------------------------------

    fn publish(dir: &Path) -> Result<()> {
        let run = dir.join(format!("orbit-spike-{}", std::process::id()));
        fs::create_dir(&run)?;
        println!("volume: {}", volume(&run)?);
        println!("umask: {:03o}", umask());

        // Raw capabilities of the volume, outside tempfile.
        raw_checks(&run)?;

        // P4: mode of a temporary file and of the published file.
        let temp = temp_in(&run, b"free")?;
        println!(
            "P4 temp {} mode {:o}",
            name(temp.path()),
            mode(temp.path())?
        );
        let free = run.join("free.zip");
        let temp_path = temp.path().to_path_buf();
        match temp.persist_noclobber(&free) {
            Ok(_) => println!(
                "P4 free name: published, mode {:o}, temp name left: {}",
                mode(&free)?,
                temp_path.exists()
            ),
            Err(err) => println!("P4 free name: FAILED {}", describe(&err.error)),
        }

        // P1: the candidate is taken by a file, a folder, a dangling symlink.
        type Make = fn(&Path) -> io::Result<()>;
        let occupants: [(&str, Make); 3] = [
            ("file", |path| fs::write(path, b"old")),
            ("folder", |path| fs::create_dir(path)),
            ("dangling symlink", |path| symlink("nowhere", path)),
        ];
        for (label, make) in occupants {
            let taken = run.join(format!("taken {label}.zip"));
            make(&taken)?;
            let before = snapshot(&taken)?;
            let temp = temp_in(&run, b"new")?;
            match temp.persist_noclobber(&taken) {
                Ok(_) => println!("P1 {label}: OVERWRITTEN, now {}", snapshot(&taken)?),
                Err(err) => {
                    let after = snapshot(&taken)?;
                    let retry = run.join(format!("taken {label} 2.zip"));
                    let retried = match err.file.persist_noclobber(&retry) {
                        Ok(_) => format!("retry ok ({})", snapshot(&retry)?),
                        Err(again) => format!("retry FAILED {}", describe(&again.error)),
                    };
                    println!(
                        "P1 {label}: {} | occupant unchanged: {} | {retried}",
                        describe(&err.error),
                        before == after
                    );
                }
            }
        }

        // P3: a 255-byte name is taken, its " 2" candidate is 257 bytes.
        let base = "a".repeat(251);
        let first = run.join(format!("{base}.zip"));
        let second = run.join(format!("{base} 2.zip"));
        fs::write(&first, b"old")?;
        let temp = temp_in(&run, b"new")?;
        match temp.persist_noclobber(&first) {
            Ok(_) => println!("P3 first: OVERWRITTEN"),
            Err(err) => {
                println!("P3 first (255 bytes): {}", describe(&err.error));
                match err.file.persist_noclobber(&second) {
                    Ok(_) => println!("P3 second (257 bytes): published?!"),
                    Err(err) => println!("P3 second (257 bytes): {}", describe(&err.error)),
                }
            }
        }
        println!("P3 first unchanged: {}", fs::read(&first)? == b"old");

        println!("leftovers: {:?}", leftovers(&run)?);
        Ok(())
    }

    /// `renamex_np(RENAME_EXCL)` and `link` on this volume, as tempfile
    /// would try them.
    fn raw_checks(run: &Path) -> Result<()> {
        let from = run.join("raw-from");
        let to = run.join("raw-to");
        fs::write(&from, b"x")?;
        let rename = unsafe_renamex_excl(&from, &to);
        println!("raw renamex_np(RENAME_EXCL) to a free name: {rename}");
        let source = if to.exists() { &to } else { &from };
        let link = match fs::hard_link(source, run.join("raw-link")) {
            Ok(()) => "ok".to_owned(),
            Err(err) => describe(&err),
        };
        println!("raw link: {link}");
        Ok(())
    }

    fn unsafe_renamex_excl(from: &Path, to: &Path) -> String {
        let (Ok(from), Ok(to)) = (c_path(from), c_path(to)) else {
            return "path with NUL".into();
        };
        // SAFETY: both pointers are valid NUL-terminated strings.
        let code = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        if code == 0 {
            "ok".into()
        } else {
            describe(&io::Error::last_os_error())
        }
    }

    fn temp_in(dir: &Path, content: &[u8]) -> io::Result<NamedTempFile> {
        let mut temp = Builder::new()
            .prefix(".orbit-")
            .suffix(".tmp")
            .permissions(Permissions::from_mode(0o666))
            .tempfile_in(dir)?;
        temp.write_all(content)?;
        temp.flush()?;
        Ok(temp)
    }

    /// What lies at `path`: kind and content or link target.
    fn snapshot(path: &Path) -> io::Result<String> {
        let meta = fs::symlink_metadata(path)?;
        Ok(if meta.file_type().is_symlink() {
            format!("symlink -> {}", fs::read_link(path)?.display())
        } else if meta.is_dir() {
            format!("folder ino {}", meta.ino())
        } else {
            format!("file {:?}", String::from_utf8_lossy(&fs::read(path)?))
        })
    }

    fn leftovers(dir: &Path) -> io::Result<Vec<String>> {
        let mut names: Vec<String> = fs::read_dir(dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".orbit-"))
            .collect();
        names.sort();
        Ok(names)
    }

    // --- P9 --------------------------------------------------------------

    fn sync(dir: &Path, mib: usize) -> Result<()> {
        println!("volume: {}", volume(dir)?);
        let mut temp = Builder::new()
            .prefix(".orbit-")
            .suffix(".tmp")
            .tempfile_in(dir)?;
        let mut block = vec![0_u8; 1 << 20];
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        for _ in 0..mib {
            fill_random(&mut block, &mut state);
            temp.write_all(&block)?;
        }
        temp.flush()?;
        let start = Instant::now();
        let full = temp.as_file().sync_all();
        let full_time = start.elapsed();
        let start = Instant::now();
        // SAFETY: the descriptor belongs to the open temporary file.
        let code = unsafe { libc::fsync(std::os::fd::AsRawFd::as_raw_fd(temp.as_file())) };
        let plain = if code == 0 {
            "ok".to_owned()
        } else {
            describe(&io::Error::last_os_error())
        };
        println!(
            "P9 {mib} MiB: sync_all (F_FULLFSYNC) {} in {full_time:?}; fsync again {plain} in {:?}",
            full.as_ref().map_or_else(describe, |()| "ok".to_owned()),
            start.elapsed()
        );
        Ok(())
    }

    // --- P5–P8 -----------------------------------------------------------

    /// How the spike archives.
    #[derive(Default, Clone, Copy)]
    struct Opts {
        /// Deflate level; the default of the encoder when `None`.
        level: Option<i64>,
        /// Adds `__MACOSX/…/._name` AppleDouble entries made by
        /// `fcopyfile(COPYFILE_PACK)` for objects with extended attributes.
        pack: bool,
    }

    #[derive(Default)]
    struct Stats {
        sidecars: u64,
        files: u64,
        dirs: u64,
        links: u64,
        skipped: u64,
        bytes: u64,
    }

    fn archive(src: &Path, out: &Path, opts: Opts) -> Result<()> {
        let start = Instant::now();
        let file = File::create_new(out)?;
        let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, file)).set_auto_large_file();
        let mut stats = Stats::default();
        let root = src
            .file_name()
            .ok_or("the source has no name")?
            .to_str()
            .ok_or("the source name is not UTF-8")?
            .to_owned();
        add(&mut zip, src, &root, opts, &mut stats)?;
        let writer = zip.finish()?;
        let file = writer.into_inner().map_err(|err| err.into_error())?;
        let written = file.metadata()?.len();
        let elapsed = start.elapsed();
        println!(
            "zip: {} files, {} dirs, {} links, {} sidecars, {} skipped, {:.1} MiB in → {:.1} MiB out, {elapsed:?}, max RSS {:.1} MiB",
            stats.files,
            stats.dirs,
            stats.links,
            stats.sidecars,
            stats.skipped,
            mib(stats.bytes),
            mib(written),
            mib(max_rss())
        );
        Ok(())
    }

    fn add(
        zip: &mut ZipWriter<BufWriter<File>>,
        path: &Path,
        name: &str,
        opts: Opts,
        stats: &mut Stats,
    ) -> Result<()> {
        let meta = fs::symlink_metadata(path)?;
        let options = options(&meta, opts.level)?;
        if opts.pack && (meta.is_dir() || meta.is_file()) {
            let source = File::open(path)?;
            if let Some(sidecar) = apple_double(&source)? {
                zip.start_file(sidecar_name(name), options.clone())?;
                zip.write_all(&sidecar)?;
                stats.sidecars += 1;
            }
        }
        let kind = meta.file_type();
        if kind.is_symlink() {
            let target = fs::read_link(path)?;
            let target = target.to_str().ok_or("link target is not UTF-8")?;
            zip.add_symlink(name, target, options)?;
            stats.links += 1;
        } else if kind.is_dir() {
            zip.add_directory(format!("{name}/"), options)?;
            stats.dirs += 1;
            let mut children: Vec<_> = fs::read_dir(path)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<_>>()?;
            children.sort();
            for child in children {
                let child_name = child.to_str().ok_or("name is not UTF-8")?;
                add(
                    zip,
                    &path.join(&child),
                    &format!("{name}/{child_name}"),
                    opts,
                    stats,
                )?;
            }
        } else if kind.is_file() {
            zip.start_file(name, options)?;
            let mut file = File::open(path)?;
            let mut buffer = vec![0_u8; 256 << 10];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                zip.write_all(buffer.get(..read).ok_or("read past the buffer")?)?;
                stats.bytes += read as u64;
            }
            stats.files += 1;
        } else {
            stats.skipped += 1;
        }
        Ok(())
    }

    /// The AppleDouble form of the extended attributes and ACL of an open
    /// object, or `None` when it has no extended attributes.
    fn apple_double(source: &File) -> Result<Option<Vec<u8>>> {
        use std::io::{Seek, SeekFrom};
        use std::os::fd::AsRawFd;

        let fd = source.as_raw_fd();
        // SAFETY: a null buffer of size 0 asks only for the size.
        let size = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0, 0) };
        if size < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if size == 0 {
            return Ok(None);
        }
        let mut packed = tempfile::tempfile()?;
        let flags = libc::COPYFILE_PACK | libc::COPYFILE_XATTR | libc::COPYFILE_ACL;
        // SAFETY: both descriptors are open for the call; a null state is
        // allowed.
        let code = unsafe { libc::fcopyfile(fd, packed.as_raw_fd(), std::ptr::null_mut(), flags) };
        if code != 0 {
            return Err(io::Error::last_os_error().into());
        }
        packed.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        packed.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    /// `__MACOSX/<folder>/._<name>`, where ditto puts the sidecar.
    fn sidecar_name(name: &str) -> String {
        match name.rsplit_once('/') {
            Some((folder, base)) => format!("__MACOSX/{folder}/._{base}"),
            None => format!("__MACOSX/._{name}"),
        }
    }

    fn options(
        meta: &fs::Metadata,
        level: Option<i64>,
    ) -> Result<FullFileOptions<'static, 'static>> {
        let modified = meta.modified()?;
        let seconds = modified
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u32::try_from(elapsed.as_secs()).ok());
        let options = FullFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(level)
            .unix_permissions(meta.mode() & 0o777)
            .last_modified_time(local_dos_time(modified))
            // `set_auto_large_file` only covers data descriptors of a
            // streamed archive; a seekable one needs the flag per entry.
            .large_file(meta.len() >= 1 << 31)
            .with_extended_timestamp(seconds, None, None)?;
        Ok(options)
    }

    fn local_dos_time(time: SystemTime) -> DateTime {
        let Ok(elapsed) = time.duration_since(UNIX_EPOCH) else {
            return DateTime::default();
        };
        let Ok(seconds) = libc::time_t::try_from(elapsed.as_secs()) else {
            return DateTime::default();
        };
        // SAFETY: `tm` is plain data that localtime_r fills.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are valid for the call.
        if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
            return DateTime::default();
        }
        let part = |value: libc::c_int| u8::try_from(value).ok();
        let year = u16::try_from(tm.tm_year + 1900).ok();
        match (
            year,
            part(tm.tm_mon + 1),
            part(tm.tm_mday),
            part(tm.tm_hour),
            part(tm.tm_min),
            part(tm.tm_sec),
        ) {
            (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) => {
                DateTime::from_date_and_time(y, mo, d, h, mi, s).unwrap_or_default()
            }
            _ => DateTime::default(),
        }
    }

    // --- P8 inputs -------------------------------------------------------

    fn random(dir: &Path, count: usize, mib: usize) -> Result<()> {
        fs::create_dir_all(dir)?;
        let mut block = vec![0_u8; 1 << 20];
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        for index in 0..count {
            let mut file =
                BufWriter::new(File::create_new(dir.join(format!("random-{index}.bin")))?);
            for _ in 0..mib {
                fill_random(&mut block, &mut state);
                file.write_all(&block)?;
            }
            file.flush()?;
        }
        println!("{count} files of {mib} MiB in {}", dir.display());
        Ok(())
    }

    fn fill(dir: &Path, count: usize) -> Result<()> {
        fs::create_dir_all(dir)?;
        for index in 0..count {
            File::create_new(dir.join(format!("f{index:07}")))?;
        }
        println!("{count} empty files in {}", dir.display());
        Ok(())
    }

    /// xorshift64*: incompressible enough for deflate.
    fn fill_random(block: &mut [u8], state: &mut u64) {
        for chunk in block.chunks_mut(8) {
            *state ^= *state >> 12;
            *state ^= *state << 25;
            *state ^= *state >> 27;
            let value = state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
            for (byte, value) in chunk.iter_mut().zip(value) {
                *byte = value;
            }
        }
    }

    // --- helpers ---------------------------------------------------------

    fn volume(path: &Path) -> Result<String> {
        let path = c_path(path)?;
        // SAFETY: `statfs` is plain data that statfs fills.
        let mut info: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are valid for the call.
        if unsafe { libc::statfs(path.as_ptr(), &mut info) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: the kernel NUL-terminates both names.
        let kind = unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) };
        // SAFETY: as above.
        let mount = unsafe { CStr::from_ptr(info.f_mntonname.as_ptr()) };
        Ok(format!(
            "{} at {}",
            kind.to_string_lossy(),
            mount.to_string_lossy()
        ))
    }

    fn umask() -> u32 {
        // SAFETY: umask has no preconditions; the old mask is put back at
        // once, and the spike runs on one thread.
        let old = unsafe { libc::umask(0o022) };
        // SAFETY: as above.
        unsafe { libc::umask(old) };
        u32::from(old)
    }

    fn mode(path: &Path) -> io::Result<u32> {
        Ok(fs::symlink_metadata(path)?.permissions().mode() & 0o7777)
    }

    fn describe(err: &io::Error) -> String {
        format!("{:?} (errno {:?}: {err})", err.kind(), err.raw_os_error())
    }

    fn c_path(path: &Path) -> Result<CString> {
        Ok(CString::new(path.as_os_str().as_bytes())?)
    }

    fn name(path: &Path) -> String {
        path.file_name().map_or_else(String::new, |name| {
            OsStr::to_string_lossy(name).into_owned()
        })
    }

    fn max_rss() -> u64 {
        // SAFETY: `rusage` is plain data that getrusage fills.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: the pointer is valid for the call.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return 0;
        }
        // Bytes on macOS.
        u64::try_from(usage.ru_maxrss).unwrap_or(0)
    }

    #[expect(clippy::cast_precision_loss, reason = "a rounded figure for a report")]
    fn mib(bytes: u64) -> f64 {
        bytes as f64 / f64::from(1 << 20)
    }
}
