//! What the info panel says about a target: its name, a few facts and its
//! path, as read when the gesture started. Nothing here reads the disk: the
//! size of a folder is counted elsewhere and written in with [`size_text`].

use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2_foundation::{
    NSByteCountFormatter, NSByteCountFormatterCountStyle, NSDate, NSDateFormatter,
    NSDateFormatterStyle,
};
use orbit_domain::target::{FileKind, FileTarget, ImageFacts, ImageFormat};

use crate::platform::folder_size::FolderSize;

/// Where the Size row is among the facts, when there is one.
pub(super) const SIZE_ROW: usize = 1;

/// The text of the info panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InfoSheet {
    /// The name of the object, or the whole path when it has none (`/`).
    pub title: String,
    /// Short facts, one line each, in the order shown.
    pub facts: Vec<InfoRow>,
    /// The full path, shown last: it may take several lines.
    pub path: String,
    /// The size is still to be counted (a folder or a package): the Size
    /// row says so until the count is written in.
    pub counting: bool,
}

/// One fact: a label and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InfoRow {
    pub label: &'static str,
    pub value: String,
}

/// How sizes and dates are written.
pub(super) trait Formats {
    /// A size in bytes, with the exact count: "1.2 MB (1,234,567 bytes)".
    fn bytes(&self, size: u64) -> String;
    /// A size in bytes, rounded only: "1.2 MB".
    fn bytes_short(&self, size: u64) -> String;
    /// A moment.
    fn date(&self, at: SystemTime) -> String;
}

/// The sheet for `target`: Kind, Size (not for a symbolic link), Modified
/// and Image (images only), then the path. The size of a folder or a
/// package is not known yet; its row starts as "Calculating…".
pub(super) fn info_sheet(target: &FileTarget, formats: &impl Formats) -> InfoSheet {
    let path = target.path.to_string_lossy().into_owned();
    let title = target
        .path
        .file_name()
        .map_or_else(|| path.clone(), |name| name.to_string_lossy().into_owned());

    let mut facts = Vec::with_capacity(4);
    facts.push(row("Kind", kind_name(target.facts.kind)));
    let counting = matches!(target.facts.kind, FileKind::Directory | FileKind::Package);
    match target.facts.kind {
        FileKind::File => facts.push(row("Size", formats.bytes(target.version.size))),
        FileKind::Directory | FileKind::Package => {
            facts.push(row("Size", size_text(&SizeState::Counting(None), formats)));
        }
        FileKind::Symlink => {}
    }
    facts.push(row("Modified", formats.date(target.version.modified)));
    if let Some(image) = target.facts.image {
        facts.push(row("Image", image_text(image)));
    }
    InfoSheet {
        title,
        facts,
        path,
        counting,
    }
}

/// Where the count of a folder is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SizeState {
    /// Still counting; the sum so far, once there is one to show.
    Counting(Option<u64>),
    Done(FolderSize),
}

/// The value of the Size row of a folder. A running sum and a sum with
/// unreadable parts are rounded only, to stay on one line.
pub(super) fn size_text(state: &SizeState, formats: &impl Formats) -> String {
    match *state {
        SizeState::Counting(None) => "Calculating…".to_owned(),
        SizeState::Counting(Some(bytes)) => format!("Calculating… {}", formats.bytes_short(bytes)),
        SizeState::Done(FolderSize {
            bytes,
            unreadable: 0,
        }) => formats.bytes(bytes),
        SizeState::Done(FolderSize { bytes, .. }) => {
            format!("At least {}", formats.bytes_short(bytes))
        }
    }
}

fn row(label: &'static str, value: impl Into<String>) -> InfoRow {
    InfoRow {
        label,
        value: value.into(),
    }
}

/// How Orbit classifies the object; not the document type Finder shows.
fn kind_name(kind: FileKind) -> &'static str {
    match kind {
        FileKind::File => "File",
        FileKind::Directory => "Folder",
        FileKind::Package => "Package",
        FileKind::Symlink => "Symbolic link",
    }
}

/// `JPEG, 3000 × 4000`; the size as displayed, after the EXIF orientation.
fn image_text(image: ImageFacts) -> String {
    let size = format!("{} × {}", image.size.width, image.size.height);
    match image.format {
        ImageFormat::Jpeg => format!("JPEG, {size}"),
        ImageFormat::Png => format!("PNG, {size}"),
        ImageFormat::Other => size,
    }
}

/// [`Formats`] of the user's locale, as Finder writes them.
pub(super) struct MacFormats {
    bytes: Retained<NSByteCountFormatter>,
    bytes_short: Retained<NSByteCountFormatter>,
    date: Retained<NSDateFormatter>,
}

impl MacFormats {
    /// Formatters are costly to make, so one set lives with the panel.
    pub(super) fn new() -> Self {
        let bytes = NSByteCountFormatter::new();
        bytes.setCountStyle(NSByteCountFormatterCountStyle::File);
        // "1.2 MB (1,234,567 bytes)", like Get Info.
        bytes.setIncludesActualByteCount(true);
        let bytes_short = NSByteCountFormatter::new();
        bytes_short.setCountStyle(NSByteCountFormatterCountStyle::File);
        let date = NSDateFormatter::new();
        date.setDateStyle(NSDateFormatterStyle::MediumStyle);
        date.setTimeStyle(NSDateFormatterStyle::ShortStyle);
        Self {
            bytes,
            bytes_short,
            date,
        }
    }
}

impl Formats for MacFormats {
    fn bytes(&self, size: u64) -> String {
        self.bytes.stringFromByteCount(byte_count(size)).to_string()
    }

    fn bytes_short(&self, size: u64) -> String {
        self.bytes_short
            .stringFromByteCount(byte_count(size))
            .to_string()
    }

    fn date(&self, at: SystemTime) -> String {
        let seconds = match at.duration_since(UNIX_EPOCH) {
            Ok(after) => after.as_secs_f64(),
            Err(before) => -before.duration().as_secs_f64(),
        };
        let date = NSDate::dateWithTimeIntervalSince1970(seconds);
        self.date.stringFromDate(&date).to_string()
    }
}

/// `size` as Foundation takes it; past `i64::MAX` it is capped.
fn byte_count(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use orbit_domain::target::{FileFacts, FileIdentity, FileVersion, PixelSize};

    use super::*;

    /// Writes what it was given, so a test sees which value went where.
    struct Plain;

    impl Formats for Plain {
        fn bytes(&self, size: u64) -> String {
            format!("{size} B")
        }

        fn bytes_short(&self, size: u64) -> String {
            format!("~{size} B")
        }

        fn date(&self, at: SystemTime) -> String {
            let seconds = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            format!("@{seconds}")
        }
    }

    fn target(path: PathBuf, kind: FileKind, image: Option<ImageFacts>) -> FileTarget {
        FileTarget {
            path,
            identity: FileIdentity {
                device: 1,
                inode: 2,
            },
            version: FileVersion {
                size: 1234,
                modified: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            },
            facts: FileFacts { kind, image },
        }
    }

    fn sheet(path: &str, kind: FileKind, image: Option<ImageFacts>) -> InfoSheet {
        info_sheet(&target(PathBuf::from(path), kind, image), &Plain)
    }

    fn facts(sheet: &InfoSheet) -> Vec<(&'static str, &str)> {
        sheet
            .facts
            .iter()
            .map(|row| (row.label, row.value.as_str()))
            .collect()
    }

    fn image(format: ImageFormat) -> Option<ImageFacts> {
        PixelSize::new(3000, 4000).map(|size| ImageFacts { format, size })
    }

    #[test]
    fn a_file_has_its_kind_size_date_and_path() {
        let sheet = sheet("/Users/me/notes.txt", FileKind::File, None);
        assert_eq!(sheet.title, "notes.txt");
        assert_eq!(
            facts(&sheet),
            vec![
                ("Kind", "File"),
                ("Size", "1234 B"),
                ("Modified", "@1700000000"),
            ]
        );
        assert_eq!(sheet.path, "/Users/me/notes.txt");
        assert!(!sheet.counting);
        assert_eq!(sheet.facts[SIZE_ROW].label, "Size");
    }

    #[test]
    fn folders_and_packages_start_counting_their_size() {
        for (kind, name) in [
            (FileKind::Directory, "Folder"),
            (FileKind::Package, "Package"),
        ] {
            let sheet = sheet("/Applications/Thing.app", kind, None);
            assert_eq!(
                facts(&sheet),
                vec![
                    ("Kind", name),
                    ("Size", "Calculating…"),
                    ("Modified", "@1700000000"),
                ],
                "{kind:?}"
            );
            assert!(sheet.counting, "{kind:?}");
            assert_eq!(sheet.facts[SIZE_ROW].label, "Size");
        }
    }

    #[test]
    fn a_link_has_no_size() {
        let sheet = sheet("/tmp/link", FileKind::Symlink, None);
        assert_eq!(
            facts(&sheet),
            vec![("Kind", "Symbolic link"), ("Modified", "@1700000000")]
        );
        assert!(!sheet.counting);
    }

    #[test]
    fn the_size_of_a_folder_reads_by_its_state() {
        let text = |state| size_text(&state, &Plain);
        assert_eq!(text(SizeState::Counting(None)), "Calculating…");
        assert_eq!(
            text(SizeState::Counting(Some(5000))),
            "Calculating… ~5000 B"
        );
        assert_eq!(
            text(SizeState::Done(FolderSize {
                bytes: 5000,
                unreadable: 0
            })),
            "5000 B"
        );
        assert_eq!(
            text(SizeState::Done(FolderSize {
                bytes: 5000,
                unreadable: 3
            })),
            "At least ~5000 B"
        );
    }

    #[test]
    fn an_image_ends_with_its_format_and_displayed_size() {
        for (format, text) in [
            (ImageFormat::Jpeg, "JPEG, 3000 × 4000"),
            (ImageFormat::Png, "PNG, 3000 × 4000"),
            (ImageFormat::Other, "3000 × 4000"),
        ] {
            let sheet = sheet("/tmp/photo", FileKind::File, image(format));
            assert_eq!(sheet.facts.len(), 4, "{format:?}");
            assert_eq!(
                sheet
                    .facts
                    .last()
                    .map(|row| (row.label, row.value.as_str())),
                Some(("Image", text))
            );
        }
    }

    #[test]
    fn a_name_that_is_not_utf8_is_shown_lossily() {
        let mut path = PathBuf::from("/tmp");
        path.push(OsStr::from_bytes(b"\xff.txt"));
        let sheet = info_sheet(&target(path, FileKind::File, None), &Plain);
        assert_eq!(sheet.title, "\u{FFFD}.txt");
        assert_eq!(sheet.path, "/tmp/\u{FFFD}.txt");
    }

    #[test]
    fn the_root_is_titled_by_its_path() {
        let sheet = sheet("/", FileKind::Directory, None);
        assert_eq!(sheet.title, "/");
        assert_eq!(sheet.path, "/");
    }

    #[test]
    fn the_mac_formats_write_sizes_and_dates() {
        let formats = MacFormats::new();
        // The exact text depends on the locale; the byte count is in it.
        let size = formats.bytes(1_234_567);
        assert!(
            size.chars().filter(|c| c.is_numeric()).count() >= 7,
            "{size}"
        );
        assert!(!formats.bytes(u64::MAX).is_empty());
        assert!(formats.bytes_short(1_234_567).len() < size.len(), "{size}");
        assert!(
            !formats
                .date(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
                .is_empty()
        );
        assert!(
            !formats
                .date(UNIX_EPOCH - Duration::from_secs(86_400))
                .is_empty()
        );
    }
}
