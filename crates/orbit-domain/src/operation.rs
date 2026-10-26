//! Operation parameters, destination names and name checks.
//!
//! Nothing here touches the filesystem: names are built and checked as
//! strings, and everything that depends on the volume (length limits, which
//! names are equivalent, whether a name is taken) is left to the operation
//! when it runs.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::iter::FusedIterator;
use std::num::NonZeroU32;
use std::path::Path;

use thiserror::Error;

use crate::action::ActionId;
use crate::target::PixelSize;

/// What an operation does to its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationSpec {
    /// Create a ZIP archive of the target.
    ArchiveZip,
    /// Create a copy of an image in another format.
    ConvertImage {
        /// Format of the copy.
        format: OutputFormat,
    },
    /// Create a copy of an image with another size, in the source format.
    ResizeImage {
        /// Size of the copy.
        size: PixelSize,
    },
    /// Give the target a new name in the same folder.
    Rename {
        /// The new name, used exactly.
        new_name: FileName,
    },
}

impl OperationSpec {
    /// The operation of an action that runs as a job without parameters, or
    /// `None` for any other action.
    pub const fn for_job_action(action: ActionId) -> Option<Self> {
        match action {
            ActionId::ArchiveZip => Some(Self::ArchiveZip),
            ActionId::ConvertToJpeg => Some(Self::ConvertImage {
                format: OutputFormat::Jpeg {
                    quality: JPEG_QUALITY,
                },
            }),
            ActionId::ConvertToPng => Some(Self::ConvertImage {
                format: OutputFormat::Png,
            }),
            ActionId::CopyPath
            | ActionId::ShowInfo
            | ActionId::Rename
            | ActionId::ResizeImage
            | ActionId::Reveal => None,
        }
    }

    /// Name of the result in the folder of the source.
    ///
    /// `source_name` is the file name of the target, without its folder.
    /// Rename gets exactly the requested name; every other operation creates
    /// a new file and gets a sequence of candidates.
    pub fn destination(&self, source_name: &OsStr) -> Destination {
        let source = Path::new(source_name);
        let candidates = match self {
            Self::Rename { new_name } => return Destination::Exact(new_name.clone()),
            // Like Finder: `Docs` → `Docs.zip`, `a.txt` → `a.txt.zip`.
            Self::ArchiveZip => NameCandidates::new(source_name.to_os_string(), Some("zip".into())),
            Self::ConvertImage { format } => {
                NameCandidates::new(stem(source), Some(format.extension().into()))
            }
            Self::ResizeImage { size } => {
                let mut base = stem(source);
                base.push(format!(" {}x{}", size.width, size.height));
                NameCandidates::new(base, source.extension().map(OsStr::to_os_string))
            }
        };
        Destination::Candidates(candidates)
    }
}

/// The file name without its extension; a name like `.png` has none.
fn stem(source: &Path) -> OsString {
    source
        .file_stem()
        .map_or_else(|| source.as_os_str().to_os_string(), OsStr::to_os_string)
}

/// Format of a converted image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    /// JPEG with the given quality.
    Jpeg {
        /// Compression quality.
        quality: Quality,
    },
    /// PNG; lossless, so it has no quality.
    Png,
}

impl OutputFormat {
    /// Extension of files in this format, without the dot.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Jpeg { .. } => "jpg",
            Self::Png => "png",
        }
    }
}

/// JPEG compression quality in percent, from 1 to 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Quality(u8);

impl Quality {
    /// Returns `None` outside 1..=100.
    pub const fn new(percent: u8) -> Option<Self> {
        if percent >= 1 && percent <= 100 {
            Some(Self(percent))
        } else {
            None
        }
    }

    /// Quality in percent.
    pub const fn percent(self) -> u8 {
        self.0
    }
}

/// Quality of the "To JPEG" action.
pub const JPEG_QUALITY: Quality = Quality(90);

/// Where the result of an operation goes, inside the folder of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// Exactly this name. If it is taken, the operation fails with a
    /// conflict; no number is ever appended.
    Exact(FileName),
    /// The first free name of these candidates.
    Candidates(NameCandidates),
}

/// Most candidates [`NameCandidates`] yields.
pub const MAX_NAME_CANDIDATES: u32 = 1000;

/// Names for a new file: `<base>.<ext>`, then `<base> 2.<ext>`,
/// `<base> 3.<ext>` and so on, [`MAX_NAME_CANDIDATES`] in all.
///
/// Lengths are not checked: the limit depends on the volume. The operation
/// that publishes the result tries the candidates in order:
/// - the name is taken (`AlreadyExists`) → the next candidate;
/// - any other error, including a name that is too long, → stop with that
///   error; later candidates are only longer;
/// - no candidate left → fail because no free name was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameCandidates {
    base: OsString,
    extension: Option<OsString>,
    /// Number of the next candidate, starting at 1.
    next: u32,
}

impl NameCandidates {
    fn new(base: OsString, extension: Option<OsString>) -> Self {
        Self {
            base,
            extension,
            next: 1,
        }
    }
}

impl Iterator for NameCandidates {
    type Item = OsString;

    fn next(&mut self) -> Option<OsString> {
        if self.next > MAX_NAME_CANDIDATES {
            return None;
        }
        let mut name = self.base.clone();
        if self.next > 1 {
            name.push(format!(" {}", self.next));
        }
        if let Some(extension) = &self.extension {
            name.push(".");
            name.push(extension);
        }
        self.next += 1;
        Some(name)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = (MAX_NAME_CANDIDATES + 1).saturating_sub(self.next);
        let left = usize::try_from(left).unwrap_or(usize::MAX);
        (left, Some(left))
    }
}

impl ExactSizeIterator for NameCandidates {}

impl FusedIterator for NameCandidates {}

/// A new name for Rename that passes the structural checks.
///
/// Only rules that hold on every volume are checked here. Length limits and
/// which names the volume treats as the same (case, Unicode normalization)
/// are decided when the rename runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileName(String);

impl FileName {
    /// Checks a name typed by the user.
    ///
    /// Leading and trailing spaces are kept, and a leading dot is allowed,
    /// although Finder then hides the file.
    ///
    /// # Errors
    ///
    /// See [`NameError`]. `:` is refused on purpose: Finder shows it as `/`,
    /// so the file would not look like the name that was typed.
    pub fn new(name: &str) -> Result<Self, NameError> {
        if name.trim().is_empty() {
            Err(NameError::Empty)
        } else if name == "." || name == ".." {
            Err(NameError::Reserved)
        } else if name.contains('\0') {
            Err(NameError::ContainsNul)
        } else if name.contains('/') {
            Err(NameError::ContainsSlash)
        } else if name.contains(':') {
            Err(NameError::ContainsColon)
        } else {
            Ok(Self(name.to_owned()))
        }
    }

    /// The name as typed.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name as an OS string.
    pub fn as_os_str(&self) -> &OsStr {
        OsStr::new(&self.0)
    }

    /// Whether the name is byte for byte the current one.
    ///
    /// Names that differ only in case or Unicode normalization count as
    /// changed: whether the volume treats them as the same file is decided
    /// when the rename runs.
    pub fn is_unchanged(&self, current: &OsStr) -> bool {
        self.as_os_str() == current
    }
}

impl fmt::Display for FileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a typed name cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Error)]
pub enum NameError {
    /// The name is empty or only whitespace.
    #[error("the name is empty")]
    Empty,
    /// `.` and `..` refer to folders, not to a file.
    #[error("“.” and “..” cannot be used as names")]
    Reserved,
    /// A NUL character cannot appear in a file name.
    #[error("the name contains a NUL character")]
    ContainsNul,
    /// `/` separates folders in a path.
    #[error("the name contains “/”")]
    ContainsSlash,
    /// Finder shows `:` as `/`.
    #[error("the name contains “:”, which Finder shows as “/”")]
    ContainsColon,
}

/// The side a resize fixes; the other side keeps the proportions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResizeBound {
    /// The new width in pixels.
    Width(NonZeroU32),
    /// The new height in pixels.
    Height(NonZeroU32),
}

/// Why a resize cannot produce a size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Error)]
pub enum ResizeError {
    /// The other side would not fit in 32 bits.
    #[error("the resized image would be too large")]
    TooLarge,
}

impl PixelSize {
    /// The size with one side fixed and the other scaled in proportion,
    /// rounded to the nearest pixel and at least one pixel.
    ///
    /// # Errors
    ///
    /// [`ResizeError::TooLarge`] when the scaled side does not fit in `u32`.
    pub fn scaled_to(&self, bound: ResizeBound) -> Result<Self, ResizeError> {
        let width = u128::from(self.width.get());
        let height = u128::from(self.height.get());
        match bound {
            ResizeBound::Width(new_width) => Ok(Self {
                width: new_width,
                height: scale(height, width, new_width)?,
            }),
            ResizeBound::Height(new_height) => Ok(Self {
                width: scale(width, height, new_height)?,
                height: new_height,
            }),
        }
    }
}

/// `side × target / reference`, rounded half up, at least 1. `u128` keeps
/// the product of two `u32` values and the rounding term exact.
fn scale(side: u128, reference: u128, target: NonZeroU32) -> Result<NonZeroU32, ResizeError> {
    let scaled = (2 * side * u128::from(target.get()) + reference) / (2 * reference);
    let scaled = u32::try_from(scaled).map_err(|_| ResizeError::TooLarge)?;
    Ok(NonZeroU32::new(scaled).unwrap_or(NonZeroU32::MIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> PixelSize {
        PixelSize::new(width, height).unwrap_or_else(|| panic!("{width}x{height} has a zero side"))
    }

    fn side(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap_or_else(|| panic!("zero side"))
    }

    fn name(value: &str) -> FileName {
        FileName::new(value).unwrap_or_else(|err| panic!("{value:?}: {err}"))
    }

    /// The first `count` candidates as strings.
    fn first(op: &OperationSpec, source: &str, count: usize) -> Vec<String> {
        match op.destination(OsStr::new(source)) {
            Destination::Candidates(candidates) => candidates
                .take(count)
                .map(|name| name.to_string_lossy().into_owned())
                .collect(),
            Destination::Exact(name) => panic!("expected candidates, got {name}"),
        }
    }

    const JPEG: OperationSpec = OperationSpec::ConvertImage {
        format: OutputFormat::Jpeg {
            quality: JPEG_QUALITY,
        },
    };
    const PNG: OperationSpec = OperationSpec::ConvertImage {
        format: OutputFormat::Png,
    };

    #[test]
    fn archive_names_keep_the_full_name() {
        let cases = [
            ("Docs", "Docs.zip"),
            ("a.txt", "a.txt.zip"),
            ("archive.tar.gz", "archive.tar.gz.zip"),
            (".hidden", ".hidden.zip"),
        ];
        for (source, expected) in cases {
            assert_eq!(first(&OperationSpec::ArchiveZip, source, 1), [expected]);
        }
    }

    #[test]
    fn conversion_names_replace_the_extension() {
        assert_eq!(first(&JPEG, "photo.png", 1), ["photo.jpg"]);
        assert_eq!(first(&JPEG, "IMG.HEIC", 1), ["IMG.jpg"]);
        assert_eq!(first(&PNG, "scan.final.jpeg", 1), ["scan.final.png"]);
        assert_eq!(first(&PNG, "noext", 1), ["noext.png"]);
        // A dot file has no extension, so its whole name is the stem.
        assert_eq!(first(&JPEG, ".png", 1), [".png.jpg"]);
    }

    #[test]
    fn resize_names_add_the_size_and_keep_the_extension() {
        let op = OperationSpec::ResizeImage {
            size: size(1000, 750),
        };
        assert_eq!(first(&op, "photo.png", 1), ["photo 1000x750.png"]);
        assert_eq!(first(&op, "IMG.JPG", 1), ["IMG 1000x750.JPG"]);
        assert_eq!(first(&op, "noext", 1), ["noext 1000x750"]);
    }

    #[test]
    fn candidates_are_numbered_from_two() {
        assert_eq!(
            first(&OperationSpec::ArchiveZip, "Docs", 3),
            ["Docs.zip", "Docs 2.zip", "Docs 3.zip"]
        );
        let op = OperationSpec::ResizeImage { size: size(10, 20) };
        assert_eq!(
            first(&op, "p.png", 3),
            ["p 10x20.png", "p 10x20 2.png", "p 10x20 3.png"]
        );
        assert_eq!(first(&PNG, "noext", 2), ["noext.png", "noext 2.png"]);
    }

    #[test]
    fn candidates_end_after_the_limit() {
        let Destination::Candidates(mut candidates) =
            OperationSpec::ArchiveZip.destination(OsStr::new("Docs"))
        else {
            panic!("expected candidates");
        };
        assert_eq!(candidates.len(), 1000);
        let last = candidates.by_ref().last();
        assert_eq!(last.as_deref(), Some(OsStr::new("Docs 1000.zip")));
        assert_eq!(candidates.len(), 0);
        assert_eq!(candidates.next(), None);
        assert_eq!(candidates.next(), None);
    }

    #[test]
    fn long_names_are_left_to_the_volume() {
        // 130 Cyrillic letters are 260 bytes of UTF-8; APFS accepts them.
        let long = "я".repeat(130);
        assert_eq!(long.len(), 260);
        let expected = format!("{long}.zip");
        assert_eq!(first(&OperationSpec::ArchiveZip, &long, 1), [expected]);
        assert_eq!(name(&long).as_str(), long);
    }

    #[test]
    fn rename_uses_exactly_the_requested_name() {
        let op = OperationSpec::Rename {
            new_name: name("report.txt"),
        };
        assert_eq!(
            op.destination(OsStr::new("draft.txt")),
            Destination::Exact(name("report.txt"))
        );
    }

    #[test]
    fn job_actions_map_to_operations() {
        assert_eq!(
            OperationSpec::for_job_action(ActionId::ArchiveZip),
            Some(OperationSpec::ArchiveZip)
        );
        assert_eq!(
            OperationSpec::for_job_action(ActionId::ConvertToJpeg),
            Some(JPEG)
        );
        assert_eq!(
            OperationSpec::for_job_action(ActionId::ConvertToPng),
            Some(PNG)
        );
        for action in [
            ActionId::CopyPath,
            ActionId::ShowInfo,
            ActionId::Rename,
            ActionId::ResizeImage,
            ActionId::Reveal,
        ] {
            assert_eq!(OperationSpec::for_job_action(action), None, "{action:?}");
        }
    }

    #[test]
    fn file_name_errors() {
        let cases = [
            ("", NameError::Empty),
            ("   ", NameError::Empty),
            ("\u{202F}", NameError::Empty),
            (".", NameError::Reserved),
            ("..", NameError::Reserved),
            ("a\0b", NameError::ContainsNul),
            ("a/b", NameError::ContainsSlash),
            ("/", NameError::ContainsSlash),
            ("a:b", NameError::ContainsColon),
        ];
        for (value, expected) in cases {
            assert_eq!(FileName::new(value), Err(expected), "{value:?}");
        }
    }

    #[test]
    fn file_name_accepts_valid_names() {
        for value in [
            "report.txt",
            ".hidden",
            "...",
            "Отчёт за год.pdf",
            " leading and trailing ",
            "10\u{202F}PM.png",
            "noext",
        ] {
            assert_eq!(name(value).as_str(), value);
        }
    }

    #[test]
    fn unchanged_means_byte_equal() {
        let current = OsStr::new("Report.txt");
        assert!(name("Report.txt").is_unchanged(current));
        assert!(!name("report.txt").is_unchanged(current));
        // The same file on APFS, but different strings.
        assert!(!name("ς.txt").is_unchanged(OsStr::new("Σ.txt")));
        // NFC and NFD forms of "é".
        assert!(!name("caf\u{E9}").is_unchanged(OsStr::new("cafe\u{301}")));
    }

    #[test]
    fn quality_bounds() {
        assert_eq!(Quality::new(0), None);
        assert_eq!(Quality::new(1).map(Quality::percent), Some(1));
        assert_eq!(Quality::new(100).map(Quality::percent), Some(100));
        assert_eq!(Quality::new(101), None);
        assert_eq!(JPEG_QUALITY.percent(), 90);
    }

    #[test]
    fn scaled_to_keeps_proportions() {
        let photo = size(4000, 3000);
        assert_eq!(
            photo.scaled_to(ResizeBound::Width(side(1000))),
            Ok(size(1000, 750))
        );
        assert_eq!(
            photo.scaled_to(ResizeBound::Height(side(750))),
            Ok(size(1000, 750))
        );
        // 3000 × 333 / 4000 = 249.75 → 250.
        assert_eq!(
            photo.scaled_to(ResizeBound::Width(side(333))),
            Ok(size(333, 250))
        );
        // Exactly half a pixel rounds up: 3 × 1 / 2 = 1.5 → 2.
        assert_eq!(
            size(2, 3).scaled_to(ResizeBound::Width(side(1))),
            Ok(size(1, 2))
        );
    }

    #[test]
    fn scaled_to_never_goes_below_one_pixel() {
        assert_eq!(
            size(10_000, 1).scaled_to(ResizeBound::Width(side(1))),
            Ok(size(1, 1))
        );
        assert_eq!(
            size(1, 10_000).scaled_to(ResizeBound::Height(side(1))),
            Ok(size(1, 1))
        );
    }

    #[test]
    fn scaled_to_rejects_sizes_beyond_u32() {
        assert_eq!(
            size(1, 10_000).scaled_to(ResizeBound::Width(side(1_000_000))),
            Err(ResizeError::TooLarge)
        );
        assert_eq!(
            size(1, 1).scaled_to(ResizeBound::Width(side(u32::MAX))),
            Ok(size(u32::MAX, u32::MAX))
        );
        assert_eq!(
            size(1, 2).scaled_to(ResizeBound::Width(side(u32::MAX))),
            Err(ResizeError::TooLarge)
        );
        assert_eq!(
            size(u32::MAX, u32::MAX).scaled_to(ResizeBound::Height(side(1))),
            Ok(size(1, 1))
        );
    }
}
