//! The Orbit item in the menu bar: the jobs that run, with Cancel, and
//! Quit.
//!
//! Rows are inserted, retitled and removed in place, so an open menu
//! changes under the pointer without being rebuilt or closed. The item
//! shows a ring of progress while shown jobs know their amount done, and a
//! symbol while they don't; it is redrawn only when the whole percent
//! changes.

use std::sync::Arc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSImage, NSLineCapStyle, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, ns_string};
use orbit_app::messages::{AppEvent, JobId};
use orbit_app::ports::EventSink;
use tracing::warn;

use super::jobs::JobsView;
use super::jobs_indicator::{Icon, JobRow};

/// The title of the item with no job shown.
const IDLE_TITLE: &str = "◎";
/// The symbol of the item while shown jobs don't know their amount done.
const BUSY_SYMBOL: &str = "circle.dashed";
/// Side of the ring, in points.
const RING_SIZE: f64 = 16.0;
const RING_LINE: f64 = 2.0;
/// Opacity of the track under the arc; the image is a template, so only
/// the opacity counts.
const TRACK_ALPHA: f64 = 0.3;

define_class!(
    /// The target of the Cancel rows: sends the cancellation of the job in
    /// the tag of the row.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OrbitJobCancelTarget"]
    #[ivars = Arc<dyn EventSink>]
    struct CancelTarget;

    impl CancelTarget {
        #[unsafe(method(cancelJob:))]
        fn cancel_job(&self, sender: &NSMenuItem) {
            match u64::try_from(sender.tag()) {
                Ok(raw) => self.ivars().send(AppEvent::CancelJobRequested {
                    job: JobId::new(raw),
                }),
                Err(_) => warn!(tag = sender.tag(), "a Cancel row without a job"),
            }
        }
    }
);

impl CancelTarget {
    fn new(mtm: MainThreadMarker, sink: Arc<dyn EventSink>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(sink);
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// The rows of one shown job.
struct Row {
    job: JobId,
    text: String,
    label: Retained<NSMenuItem>,
    cancel: Retained<NSMenuItem>,
}

/// The item in the menu bar and its menu. The item stays while this value
/// lives.
pub struct StatusMenu {
    mtm: MainThreadMarker,
    item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    /// Menu items keep their target weakly.
    target: Retained<CancelTarget>,
    rows: Vec<Row>,
    /// Between the rows and Quit while there are rows.
    separator: Retained<NSMenuItem>,
    separated: bool,
    icon: Icon,
}

impl StatusMenu {
    /// Adds the item to the menu bar, with a menu to quit. Cancel rows send
    /// their cancellation to `sink`.
    pub fn new(mtm: MainThreadMarker, sink: Arc<dyn EventSink>) -> Self {
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let menu = NSMenu::new(mtm);
        // SAFETY: `terminate:` is an NSApplication action; with no target
        // it goes up the responder chain to NSApp.
        let quit = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Quit Orbit"),
                Some(sel!(terminate:)),
                ns_string!("q"),
            )
        };
        menu.addItem(&quit);
        item.setMenu(Some(&menu));
        let this = Self {
            mtm,
            item,
            menu,
            target: CancelTarget::new(mtm, sink),
            rows: Vec::new(),
            separator: NSMenuItem::separatorItem(mtm),
            separated: false,
            icon: Icon::Idle,
        };
        this.draw_icon();
        this
    }

    /// The item in the menu bar, for checks of what it shows.
    pub fn item(&self) -> &NSStatusItem {
        &self.item
    }

    fn row(&self, row: &JobRow) -> Row {
        let mtm = self.mtm;
        let label = NSMenuItem::new(mtm);
        label.setTitle(&NSString::from_str(&row.text));
        // No action: the menu shows the row disabled.
        // SAFETY: `cancelJob:` is a method of the target, which takes the
        // sender.
        let cancel = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Cancel"),
                Some(sel!(cancelJob:)),
                ns_string!(""),
            )
        };
        cancel.setIndentationLevel(1);
        match isize::try_from(row.job.get()) {
            Ok(tag) => {
                cancel.setTag(tag);
                let target: &AnyObject = &self.target;
                // SAFETY: the target answers `cancelJob:`, and this menu
                // keeps it alive as long as the item.
                unsafe { cancel.setTarget(Some(target)) };
            }
            Err(_) => {
                // Without a target the menu shows the row disabled.
                warn!(
                    job = row.job.get(),
                    "the job id doesn't fit a tag; no Cancel"
                );
                // SAFETY: no action at all.
                unsafe { cancel.setAction(None) };
            }
        }
        Row {
            job: row.job,
            text: row.text.clone(),
            label,
            cancel,
        }
    }

    fn insert(&self, item: &NSMenuItem, index: usize) {
        // Never past the end: an index out of range raises.
        let index = isize::try_from(index)
            .unwrap_or(isize::MAX)
            .min(self.menu.numberOfItems());
        self.menu.insertItem_atIndex(item, index);
    }

    fn draw_icon(&self) {
        let Some(button) = self.item.button(self.mtm) else {
            return;
        };
        let image = match self.icon {
            Icon::Idle => None,
            Icon::Busy => {
                let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(BUSY_SYMBOL),
                    Some(ns_string!("Orbit is working")),
                );
                if image.is_none() {
                    warn!(symbol = BUSY_SYMBOL, "no such SF Symbol");
                }
                image
            }
            Icon::Ring(percent) => Some(ring(percent)),
        };
        if let Some(image) = &image {
            image.setTemplate(true);
            button.setTitle(ns_string!(""));
        } else {
            button.setTitle(&NSString::from_str(IDLE_TITLE));
        }
        button.setImage(image.as_deref());
    }
}

impl JobsView for StatusMenu {
    fn show(&mut self, rows: &[JobRow], icon: Icon) {
        let menu = &self.menu;
        self.rows.retain(|row| {
            let keep = rows.iter().any(|wanted| wanted.job == row.job);
            if !keep {
                menu.removeItem(&row.label);
                menu.removeItem(&row.cancel);
            }
            keep
        });
        // Jobs keep their order, so the rows left are in place.
        for (index, wanted) in rows.iter().enumerate() {
            match self.rows.get_mut(index) {
                Some(row) if row.job == wanted.job => {
                    if row.text != wanted.text {
                        row.label.setTitle(&NSString::from_str(&wanted.text));
                        row.text.clone_from(&wanted.text);
                    }
                }
                _ => {
                    let row = self.row(wanted);
                    self.insert(&row.label, 2 * index);
                    self.insert(&row.cancel, 2 * index + 1);
                    self.rows.insert(index, row);
                }
            }
        }
        if self.rows.is_empty() == self.separated {
            if self.separated {
                self.menu.removeItem(&self.separator);
            } else {
                self.insert(&self.separator, 2 * self.rows.len());
            }
            self.separated = !self.separated;
        }
        if icon != self.icon {
            self.icon = icon;
            self.draw_icon();
        }
    }
}

/// A template ring, `percent` of it drawn clockwise from the top over a
/// faint track.
fn ring(percent: u8) -> Retained<NSImage> {
    let fraction = f64::from(percent.min(100)) / 100.0;
    let block = RcBlock::new(move |rect: NSRect| -> Bool {
        let center = NSPoint::new(
            rect.origin.x + rect.size.width / 2.0,
            rect.origin.y + rect.size.height / 2.0,
        );
        let radius = (rect.size.width.min(rect.size.height) - RING_LINE) / 2.0;
        let track = NSBezierPath::bezierPath();
        track.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle(
            center, radius, 0.0, 360.0,
        );
        track.setLineWidth(RING_LINE);
        NSColor::colorWithWhite_alpha(0.0, TRACK_ALPHA).setStroke();
        track.stroke();
        if fraction > 0.0 {
            let arc = NSBezierPath::bezierPath();
            arc.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
                center,
                radius,
                90.0,
                90.0 - 360.0 * fraction,
                true,
            );
            arc.setLineWidth(RING_LINE);
            arc.setLineCapStyle(NSLineCapStyle::Round);
            NSColor::blackColor().setStroke();
            arc.stroke();
        }
        Bool::YES
    });
    NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(RING_SIZE, RING_SIZE), false, &block)
}
