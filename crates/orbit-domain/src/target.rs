//! File target: path, identity and metadata.
//!
//! The platform adapter fills these values when it resolves the target; the
//! domain only stores and compares them.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::SystemTime;

use thiserror::Error;

/// The file or folder a gesture acts on, as read when the gesture started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTarget {
    /// Absolute path of the object.
    pub path: PathBuf,
    /// Which filesystem object the path pointed to.
    pub identity: FileIdentity,
    /// Metadata used to notice that the object changed after it was read.
    pub version: FileVersion,
    /// What the object is; decides which actions are available.
    pub facts: FileFacts,
}

impl FileTarget {
    /// Checks that the object now at [`FileTarget::path`] is still the one
    /// this target was read from.
    ///
    /// # Errors
    ///
    /// [`SourceChanged::Replaced`] when the path now points to another
    /// object, [`SourceChanged::MetadataChanged`] when it is the same object
    /// but its size or modification time differ.
    pub fn check_source(
        &self,
        identity: FileIdentity,
        version: FileVersion,
    ) -> Result<(), SourceChanged> {
        if identity != self.identity {
            Err(SourceChanged::Replaced)
        } else if version != self.version {
            Err(SourceChanged::MetadataChanged)
        } else {
            Ok(())
        }
    }
}

/// Identifies a filesystem object independently of its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// Device the object lives on (`st_dev`).
    pub device: u64,
    /// Inode number on that device (`st_ino`).
    pub inode: u64,
}

/// Size and modification time of an object.
///
/// This is a metadata change check, not proof that the content is the same:
/// changed bytes with a restored modification time still compare equal. For
/// a folder, the modification time only reflects changes to its direct
/// entries, not to nested content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileVersion {
    /// Size in bytes as reported by the filesystem.
    pub size: u64,
    /// Last modification time.
    pub modified: SystemTime,
}

/// Why a source no longer matches its [`FileTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SourceChanged {
    /// The path now points to a different object.
    #[error("the file was replaced by another one")]
    Replaced,
    /// The object is the same, but its size or modification time changed.
    #[error("the file was modified")]
    MetadataChanged,
}

/// What kind of object a target is and what can be done with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    /// Filesystem kind of the object.
    pub kind: FileKind,
    /// Set only when the platform can decode the object as an image.
    pub image: Option<ImageFacts>,
}

/// Filesystem kind of a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// A regular file.
    File,
    /// A folder.
    Directory,
    /// A folder that Finder presents as a single file, such as an app bundle.
    Package,
    /// A symbolic link; the target is the link itself, not what it points to.
    Symlink,
}

/// Image properties of a decodable target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageFacts {
    /// Encoding of the source file.
    pub format: ImageFormat,
    /// Size as displayed, after the EXIF orientation is applied: for
    /// orientations 5–8 the stored width and height are swapped. A requested
    /// width therefore means the width the user sees.
    pub size: PixelSize,
}

/// Encoding of a source image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    /// JPEG.
    Jpeg,
    /// PNG.
    Png,
    /// Any other format the platform can read, such as HEIC or TIFF.
    Other,
}

/// Image dimensions in pixels; both sides are at least one pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PixelSize {
    /// Width in pixels.
    pub width: NonZeroU32,
    /// Height in pixels.
    pub height: NonZeroU32,
}

impl PixelSize {
    /// Returns `None` when either side is zero.
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        match (NonZeroU32::new(width), NonZeroU32::new(height)) {
            (Some(width), Some(height)) => Some(Self { width, height }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn target() -> FileTarget {
        FileTarget {
            path: PathBuf::from("/Users/me/photo.png"),
            identity: FileIdentity {
                device: 1,
                inode: 42,
            },
            version: FileVersion {
                size: 1024,
                modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            },
            facts: FileFacts {
                kind: FileKind::File,
                image: None,
            },
        }
    }

    #[test]
    fn check_source_accepts_the_same_object() {
        let target = target();
        assert_eq!(target.check_source(target.identity, target.version), Ok(()));
    }

    #[test]
    fn check_source_detects_a_replaced_object() {
        let target = target();
        let other = FileIdentity {
            inode: 43,
            ..target.identity
        };
        assert_eq!(
            target.check_source(other, target.version),
            Err(SourceChanged::Replaced)
        );
        let other_device = FileIdentity {
            device: 2,
            ..target.identity
        };
        assert_eq!(
            target.check_source(other_device, target.version),
            Err(SourceChanged::Replaced)
        );
    }

    #[test]
    fn check_source_detects_changed_metadata() {
        let target = target();
        let resized = FileVersion {
            size: 2048,
            ..target.version
        };
        assert_eq!(
            target.check_source(target.identity, resized),
            Err(SourceChanged::MetadataChanged)
        );
        let touched = FileVersion {
            modified: target.version.modified + Duration::from_nanos(1),
            ..target.version
        };
        assert_eq!(
            target.check_source(target.identity, touched),
            Err(SourceChanged::MetadataChanged)
        );
    }

    #[test]
    fn replaced_wins_over_changed_metadata() {
        let target = target();
        let other = FileIdentity {
            inode: 7,
            ..target.identity
        };
        let resized = FileVersion {
            size: 1,
            ..target.version
        };
        assert_eq!(
            target.check_source(other, resized),
            Err(SourceChanged::Replaced)
        );
    }

    #[test]
    fn pixel_size_rejects_zero_sides() {
        assert!(PixelSize::new(0, 10).is_none());
        assert!(PixelSize::new(10, 0).is_none());
        assert!(PixelSize::new(0, 0).is_none());
        let size = PixelSize::new(4000, 3000);
        assert_eq!(
            size.map(|s| (s.width.get(), s.height.get())),
            Some((4000, 3000))
        );
    }
}
