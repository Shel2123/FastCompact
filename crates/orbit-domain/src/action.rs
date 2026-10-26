//! Actions and their availability conditions, and the fixed menu layout.

use crate::target::{FileFacts, FileKind, ImageFormat};

/// An action the radial menu offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionId {
    /// Copy the absolute path to the clipboard.
    CopyPath,
    /// Show path, type, size and other properties.
    ShowInfo,
    /// Rename the object; asks for the new name.
    Rename,
    /// Create a ZIP archive next to the object.
    ArchiveZip,
    /// Create a resized copy in the source format; asks for the size.
    ResizeImage,
    /// Create a PNG copy of an image.
    ConvertToPng,
    /// Create a JPEG copy of an image.
    ConvertToJpeg,
    /// Show the object in Finder.
    Reveal,
}

impl ActionId {
    /// Every action, each exactly once.
    pub const ALL: [Self; 8] = [
        Self::CopyPath,
        Self::ShowInfo,
        Self::Rename,
        Self::ArchiveZip,
        Self::ResizeImage,
        Self::ConvertToPng,
        Self::ConvertToJpeg,
        Self::Reveal,
    ];

    /// How the action runs once it is chosen.
    pub const fn kind(self) -> ActionKind {
        match self {
            Self::CopyPath | Self::ShowInfo | Self::Reveal => ActionKind::Instant,
            Self::ArchiveZip | Self::ConvertToPng | Self::ConvertToJpeg => ActionKind::Job,
            Self::Rename | Self::ResizeImage => ActionKind::NeedsParameters,
        }
    }

    /// Whether the action applies to an object with these facts.
    ///
    /// Only the kind and format of the object count. Permissions and free
    /// space are not checked here: they can change before the action runs,
    /// so a failure there is reported as an error of the operation.
    pub const fn availability(self, facts: &FileFacts) -> Availability {
        match self {
            Self::CopyPath | Self::ShowInfo | Self::Reveal | Self::Rename => {
                Availability::Available
            }
            Self::ArchiveZip => match facts.kind {
                FileKind::File | FileKind::Directory | FileKind::Package => Availability::Available,
                FileKind::Symlink => Availability::Unavailable(UnavailableReason::UnsupportedKind),
            },
            // The copy keeps the source format, and only JPEG and PNG can be
            // written.
            Self::ResizeImage => match facts.image {
                None => Availability::Unavailable(UnavailableReason::NotAnImage),
                Some(image) => match image.format {
                    ImageFormat::Jpeg | ImageFormat::Png => Availability::Available,
                    ImageFormat::Other => {
                        Availability::Unavailable(UnavailableReason::UnsupportedImageFormat)
                    }
                },
            },
            Self::ConvertToPng => convert_availability(facts, ImageFormat::Png),
            Self::ConvertToJpeg => convert_availability(facts, ImageFormat::Jpeg),
        }
    }
}

/// Conversion only needs to read the source; the output is always JPEG or
/// PNG.
const fn convert_availability(facts: &FileFacts, output: ImageFormat) -> Availability {
    match facts.image {
        None => Availability::Unavailable(UnavailableReason::NotAnImage),
        Some(image) => match (image.format, output) {
            (ImageFormat::Jpeg, ImageFormat::Jpeg) | (ImageFormat::Png, ImageFormat::Png) => {
                Availability::Unavailable(UnavailableReason::AlreadyInFormat)
            }
            _ => Availability::Available,
        },
    }
}

/// How an action runs once it is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// Done right away on the main thread.
    Instant,
    /// Runs as a background job without further input.
    Job,
    /// Closes the menu and asks for parameters first.
    NeedsParameters,
}

/// Whether an action can be chosen for a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Availability {
    /// The action can be chosen.
    Available,
    /// The action stays in its slot but is inactive.
    Unavailable(UnavailableReason),
}

impl Availability {
    /// Whether the action can be chosen.
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Why an action is inactive for a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnavailableReason {
    /// The action does not apply to this kind of object.
    UnsupportedKind,
    /// The object cannot be decoded as an image.
    NotAnImage,
    /// The image can be read, but its format cannot be written.
    UnsupportedImageFormat,
    /// The image is already in the requested format.
    AlreadyInFormat,
}

/// One slot of the radial menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuItem {
    /// Runs an action.
    Action(ActionId),
    /// Closes the menu without an operation.
    Cancel,
}

/// Slot of [`MenuItem::Cancel`] in [`MENU_LAYOUT`]; the standard geometry
/// places it straight down.
pub const CANCEL_SLOT: usize = 4;

/// The fixed menu, slot 0 first, going clockwise. Slots never move:
/// unavailable actions stay in place, inactive.
pub const MENU_LAYOUT: [MenuItem; 9] = [
    MenuItem::Action(ActionId::CopyPath),
    MenuItem::Action(ActionId::ShowInfo),
    MenuItem::Action(ActionId::Rename),
    MenuItem::Action(ActionId::ArchiveZip),
    MenuItem::Cancel,
    MenuItem::Action(ActionId::ResizeImage),
    MenuItem::Action(ActionId::ConvertToPng),
    MenuItem::Action(ActionId::ConvertToJpeg),
    MenuItem::Action(ActionId::Reveal),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{ImageFacts, PixelSize};

    fn facts(kind: FileKind, format: Option<ImageFormat>) -> FileFacts {
        let size = PixelSize::new(10, 10);
        FileFacts {
            kind,
            image: format
                .zip(size)
                .map(|(format, size)| ImageFacts { format, size }),
        }
    }

    #[test]
    fn layout_holds_every_action_once_and_cancel_in_its_slot() {
        for action in ActionId::ALL {
            let count = MENU_LAYOUT
                .iter()
                .filter(|item| **item == MenuItem::Action(action))
                .count();
            assert_eq!(count, 1, "{action:?}");
        }
        let cancels: Vec<usize> = MENU_LAYOUT
            .iter()
            .enumerate()
            .filter(|(_, item)| **item == MenuItem::Cancel)
            .map(|(slot, _)| slot)
            .collect();
        assert_eq!(cancels, [CANCEL_SLOT]);
    }

    #[test]
    fn kinds() {
        use ActionKind::{Instant, Job, NeedsParameters};
        let expected = [
            (ActionId::CopyPath, Instant),
            (ActionId::ShowInfo, Instant),
            (ActionId::Reveal, Instant),
            (ActionId::ArchiveZip, Job),
            (ActionId::ConvertToPng, Job),
            (ActionId::ConvertToJpeg, Job),
            (ActionId::Rename, NeedsParameters),
            (ActionId::ResizeImage, NeedsParameters),
        ];
        for (action, kind) in expected {
            assert_eq!(action.kind(), kind, "{action:?}");
        }
    }

    #[test]
    fn always_available_actions() {
        let all_kinds = [
            FileKind::File,
            FileKind::Directory,
            FileKind::Package,
            FileKind::Symlink,
        ];
        for kind in all_kinds {
            for action in [
                ActionId::CopyPath,
                ActionId::ShowInfo,
                ActionId::Reveal,
                ActionId::Rename,
            ] {
                assert!(
                    action.availability(&facts(kind, None)).is_available(),
                    "{action:?} for {kind:?}"
                );
            }
        }
    }

    #[test]
    fn archive_availability() {
        for kind in [FileKind::File, FileKind::Directory, FileKind::Package] {
            assert_eq!(
                ActionId::ArchiveZip.availability(&facts(kind, None)),
                Availability::Available,
                "{kind:?}"
            );
        }
        assert_eq!(
            ActionId::ArchiveZip.availability(&facts(FileKind::Symlink, None)),
            Availability::Unavailable(UnavailableReason::UnsupportedKind)
        );
    }

    #[test]
    fn resize_availability() {
        use UnavailableReason::{NotAnImage, UnsupportedImageFormat};
        let cases = [
            (Some(ImageFormat::Jpeg), Availability::Available),
            (Some(ImageFormat::Png), Availability::Available),
            (
                Some(ImageFormat::Other),
                Availability::Unavailable(UnsupportedImageFormat),
            ),
            (None, Availability::Unavailable(NotAnImage)),
        ];
        for (format, expected) in cases {
            assert_eq!(
                ActionId::ResizeImage.availability(&facts(FileKind::File, format)),
                expected,
                "{format:?}"
            );
        }
    }

    #[test]
    fn convert_availability() {
        use UnavailableReason::{AlreadyInFormat, NotAnImage};
        let cases = [
            (
                ActionId::ConvertToPng,
                Some(ImageFormat::Jpeg),
                Availability::Available,
            ),
            (
                ActionId::ConvertToPng,
                Some(ImageFormat::Other),
                Availability::Available,
            ),
            (
                ActionId::ConvertToPng,
                Some(ImageFormat::Png),
                Availability::Unavailable(AlreadyInFormat),
            ),
            (
                ActionId::ConvertToJpeg,
                Some(ImageFormat::Png),
                Availability::Available,
            ),
            (
                ActionId::ConvertToJpeg,
                Some(ImageFormat::Other),
                Availability::Available,
            ),
            (
                ActionId::ConvertToJpeg,
                Some(ImageFormat::Jpeg),
                Availability::Unavailable(AlreadyInFormat),
            ),
            (
                ActionId::ConvertToPng,
                None,
                Availability::Unavailable(NotAnImage),
            ),
            (
                ActionId::ConvertToJpeg,
                None,
                Availability::Unavailable(NotAnImage),
            ),
        ];
        for (action, format, expected) in cases {
            assert_eq!(
                action.availability(&facts(FileKind::File, format)),
                expected,
                "{action:?} from {format:?}"
            );
        }
    }
}
