//! Which actions are available for a target, and the model of the menu.

use orbit_domain::action::{Availability, MENU_LAYOUT, MenuItem};
use orbit_domain::geometry::{MenuGeometry, ScreenPoint};
use orbit_domain::target::FileFacts;

use crate::messages::GestureId;

/// Number of slots in the menu.
pub const SLOT_COUNT: usize = MENU_LAYOUT.len();

/// One slot of the menu for a given target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotModel {
    /// What the slot does.
    pub item: MenuItem,
    /// Whether it can be chosen; [`MenuItem::Cancel`] always can.
    pub availability: Availability,
}

/// The slots of [`MENU_LAYOUT`] with their availability for a target.
pub fn menu_slots(facts: &FileFacts) -> [SlotModel; SLOT_COUNT] {
    MENU_LAYOUT.map(|item| SlotModel {
        item,
        availability: match item {
            MenuItem::Action(action) => action.availability(facts),
            MenuItem::Cancel => Availability::Available,
        },
    })
}

/// Everything the menu view needs to draw the menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuModel {
    /// The press that opened the menu.
    pub gesture: GestureId,
    /// Menu center in Cocoa screen coordinates.
    pub center: ScreenPoint,
    /// Ring layout; the controller hit-tests with the same geometry.
    pub geometry: MenuGeometry,
    /// Name of the target, lossily converted for display.
    pub title: String,
    /// Slots in [`MENU_LAYOUT`] order.
    pub slots: [SlotModel; SLOT_COUNT],
}

#[cfg(test)]
mod tests {
    use orbit_domain::action::{ActionId, CANCEL_SLOT};
    use orbit_domain::target::{FileKind, ImageFacts, ImageFormat, PixelSize};

    use super::*;

    #[test]
    fn slots_follow_layout_and_availability() {
        let size = PixelSize::new(10, 10);
        let cases = [
            FileFacts {
                kind: FileKind::File,
                image: None,
            },
            FileFacts {
                kind: FileKind::Symlink,
                image: None,
            },
            FileFacts {
                kind: FileKind::File,
                image: size.map(|size| ImageFacts {
                    format: ImageFormat::Png,
                    size,
                }),
            },
            FileFacts {
                kind: FileKind::File,
                image: size.map(|size| ImageFacts {
                    format: ImageFormat::Other,
                    size,
                }),
            },
        ];
        for facts in cases {
            let slots = menu_slots(&facts);
            for (slot, item) in slots.iter().zip(MENU_LAYOUT) {
                assert_eq!(slot.item, item);
                let expected = match item {
                    MenuItem::Action(action) => action.availability(&facts),
                    MenuItem::Cancel => Availability::Available,
                };
                assert_eq!(slot.availability, expected, "{item:?} for {facts:?}");
            }
            assert_eq!(slots[CANCEL_SLOT].item, MenuItem::Cancel);
            assert!(slots[CANCEL_SLOT].availability.is_available());
        }
        let symlink = menu_slots(&cases[1]);
        assert!(
            symlink
                .iter()
                .any(|slot| slot.item == MenuItem::Action(ActionId::ArchiveZip)
                    && !slot.availability.is_available())
        );
    }
}
