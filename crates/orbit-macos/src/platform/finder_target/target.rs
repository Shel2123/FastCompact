//! The file target of a path.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSNumber, NSURL, NSURLIsPackageKey};
use orbit_app::messages::ResolveError;
use orbit_domain::target::{FileFacts, FileIdentity, FileKind, FileTarget, FileVersion};

/// Reads what the actions need to know about the object at `path`. A
/// symbolic link is the target itself, not what it points to.
pub(crate) fn file_target(path: &Path) -> Result<FileTarget, ResolveError> {
    let failed = |reason: String| ResolveError::Failed(format!("{}: {reason}", path.display()));
    if !path.is_absolute() {
        return Err(failed("the path is not absolute".into()));
    }
    let metadata = fs::symlink_metadata(path).map_err(|err| failed(err.to_string()))?;
    let modified = metadata.modified().map_err(|err| failed(err.to_string()))?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        FileKind::Symlink
    } else if file_type.is_dir() {
        if is_package(path).map_err(failed)? {
            FileKind::Package
        } else {
            FileKind::Directory
        }
    } else if file_type.is_file() {
        FileKind::File
    } else {
        return Err(failed("unsupported file type".into()));
    };
    Ok(FileTarget {
        path: path.to_owned(),
        identity: FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        version: FileVersion {
            size: metadata.len(),
            modified,
        },
        facts: FileFacts { kind, image: None },
    })
}

/// Whether Finder shows the folder at `path` as a single file.
fn is_package(path: &Path) -> Result<bool, String> {
    let url = NSURL::from_directory_path(path).ok_or("no URL for the path")?;
    let mut value: Option<Retained<AnyObject>> = None;
    // SAFETY: the value of NSURLIsPackageKey is an NSNumber, read into an
    // optional object as the method expects.
    unsafe { url.getResourceValue_forKey_error(&mut value, NSURLIsPackageKey) }
        .map_err(|err| format!("package check failed: {}", err.localizedDescription()))?;
    let value = value.ok_or("package check returned nothing")?;
    let number = value
        .downcast::<NSNumber>()
        .map_err(|_| "package check returned no number")?;
    Ok(number.boolValue())
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::{env, process};

    use super::*;

    /// A fresh directory, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let name = format!(
                "orbit-target-{}-{}",
                process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = env::temp_dir().join(name);
            if let Err(err) = fs::create_dir(&path) {
                panic!("temp dir {}: {err}", path.display());
            }
            // Resolve /var → /private/var so the paths are canonical.
            Self(fs::canonicalize(&path).unwrap_or(path))
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn kind(path: &Path) -> Result<FileKind, ResolveError> {
        file_target(path).map(|target| target.facts.kind)
    }

    #[test]
    fn kinds_of_objects() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new();
        let file = dir.join("a.txt");
        fs::write(&file, b"hello")?;
        let folder = dir.join("folder");
        fs::create_dir(&folder)?;
        let app = dir.join("Thing.app");
        fs::create_dir(&app)?;
        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link)?;

        let target = file_target(&file)?;
        assert_eq!(target.path, file);
        assert_eq!(target.version.size, 5);
        assert_eq!(target.facts.kind, FileKind::File);
        assert_eq!(target.facts.image, None);
        assert_eq!(kind(&folder), Ok(FileKind::Directory));
        assert_eq!(kind(&app), Ok(FileKind::Package));
        assert_eq!(kind(&link), Ok(FileKind::Symlink));
        // The link is the target, not the file it points to.
        assert_ne!(file_target(&link)?.identity, target.identity);
        Ok(())
    }

    #[test]
    fn unreadable_and_unsupported_paths_fail() -> std::io::Result<()> {
        let dir = TempDir::new();
        assert!(matches!(
            kind(Path::new("relative/a.txt")),
            Err(ResolveError::Failed(_))
        ));
        assert!(matches!(
            kind(&dir.join("missing")),
            Err(ResolveError::Failed(reason)) if reason.contains("missing")
        ));
        let socket = dir.join("s");
        let _listener = UnixListener::bind(&socket)?;
        assert!(matches!(
            kind(&socket),
            Err(ResolveError::Failed(reason)) if reason.contains("unsupported")
        ));
        Ok(())
    }
}
