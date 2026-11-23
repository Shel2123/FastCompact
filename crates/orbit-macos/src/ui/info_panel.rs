//! The info panel: what Orbit knows about the target, next to the pointer.
//!
//! One window for the life of the application, laid out with frames like
//! the toast. Showing it is two steps, so that the caller can give up in
//! between without anything on screen: [`InfoPanel::prepare`] lays the
//! sheet out and finds the frame, [`InfoPanel::show`] puts the window there.
//!
//! The path may take several lines but at most [`PATH_LINES`]; a longer one
//! loses its middle to an ellipsis, so its start and the name at its end
//! stay. AppKit does not truncate the middle of a text that wraps, so the
//! cut is made here, measured with `sizeThatFits` at the width of the column.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSColor, NSEvent, NSFont, NSLineBreakMode, NSTextAlignment, NSTextField};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use orbit_domain::geometry::{ScreenPoint, ScreenRect};

use super::backdrop::Background;
use super::info_sheet::InfoSheet;
use super::overlay::{Overlay, SCREEN_MARGIN};
use super::placement::{panel_area, place_panel};
use crate::platform::coordinates::Displays;

const WIDTH: f64 = 360.0;
const PADDING: f64 = 14.0;
const CORNER_RADIUS: f64 = 12.0;
const TITLE_FONT_SIZE: f64 = 13.0;
const FONT_SIZE: f64 = 12.0;
/// Between the title and the first fact.
const TITLE_GAP: f64 = 10.0;
/// Between two facts.
const ROW_GAP: f64 = 4.0;
/// Between a label and its value.
const COLUMN_GAP: f64 = 8.0;
/// The most lines the path takes.
const PATH_LINES: usize = 6;
/// Facts besides the path; see [`super::info_sheet::info_sheet`].
const MAX_FACTS: usize = 4;
/// Taller than any panel, as the height offered to a field being measured.
const UNBOUNDED: f64 = 100_000.0;

/// The info panel window.
pub(super) struct InfoPanel {
    mtm: MainThreadMarker,
    overlay: Overlay,
    title: Retained<NSTextField>,
    facts: [(Retained<NSTextField>, Retained<NSTextField>); MAX_FACTS],
    path_label: Retained<NSTextField>,
    path: Retained<NSTextField>,
}

impl InfoPanel {
    /// A hidden panel with `background`.
    pub(super) fn new(mtm: MainThreadMarker, background: Background) -> Self {
        let overlay = Overlay::new(mtm, background, CORNER_RADIUS);
        let title = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(TITLE_FONT_SIZE)));
        title.setTextColor(Some(&NSColor::labelColor()));
        title.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
        title.setMaximumNumberOfLines(1);
        overlay.content().addSubview(&title);

        let facts = std::array::from_fn(|_| {
            let label = label_field(mtm);
            let value = value_field(mtm);
            overlay.content().addSubview(&label);
            overlay.content().addSubview(&value);
            (label, value)
        });

        let path_label = label_field(mtm);
        path_label.setStringValue(&NSString::from_str("Path"));
        let path = NSTextField::wrappingLabelWithString(&NSString::from_str(""), mtm);
        path.setFont(Some(&NSFont::systemFontOfSize(FONT_SIZE)));
        path.setTextColor(Some(&NSColor::labelColor()));
        path.setLineBreakMode(NSLineBreakMode::ByCharWrapping);
        path.setSelectable(false);
        overlay.content().addSubview(&path_label);
        overlay.content().addSubview(&path);

        Self {
            mtm,
            overlay,
            title,
            facts,
            path_label,
            path,
        }
    }

    /// Lays `sheet` out for a panel next to the pointer and returns where
    /// the panel goes, or `None` when there is no display. The window is
    /// not touched: it stays as it is, shown or hidden.
    pub(super) fn prepare(&self, sheet: &InfoSheet) -> Option<ScreenRect> {
        let anchor = NSEvent::mouseLocation();
        let anchor = ScreenPoint::new(anchor.x, anchor.y);
        let screens = Displays::current(self.mtm)
            .map(|d| d.screens)
            .unwrap_or_default();
        let area = panel_area(anchor, &screens, SCREEN_MARGIN)?;
        let width = WIDTH.min(area.width);
        let inner = (width - 2.0 * PADDING).max(0.0);

        self.title.setStringValue(&NSString::from_str(&sheet.title));
        let title_height = height_of(&self.title, inner);

        let shown = sheet.facts.len().min(MAX_FACTS);
        for (index, (label, value)) in self.facts.iter().enumerate() {
            let row = sheet.facts.get(index);
            label.setHidden(row.is_none());
            value.setHidden(row.is_none());
            if let Some(row) = row {
                label.setStringValue(&NSString::from_str(row.label));
                value.setStringValue(&NSString::from_str(&row.value));
            }
        }
        let labels = self.facts[..shown]
            .iter()
            .map(|(label, _)| label)
            .chain([&self.path_label]);
        let label_width = labels
            .map(|label| {
                label
                    .sizeThatFits(NSSize::new(inner, UNBOUNDED))
                    .width
                    .ceil()
            })
            .fold(0.0, f64::max)
            .min(inner);
        let column = (inner - label_width - COLUMN_GAP).max(0.0);

        let rows: Vec<f64> = self.facts[..shown]
            .iter()
            .map(|(label, value)| height_of(label, label_width).max(height_of(value, column)))
            .collect();
        let path_label_height = height_of(&self.path_label, label_width);
        let path_height = self.fit_path(&sheet.path, column).max(path_label_height);

        let height = PADDING
            + title_height
            + TITLE_GAP
            + rows.iter().map(|h| h + ROW_GAP).sum::<f64>()
            + path_height
            + PADDING;
        let frame = place_panel(anchor, (width, height), &screens, SCREEN_MARGIN)?;

        // From the top down; Cocoa coordinates start at the bottom. A panel
        // cut by a small display keeps its top and loses its bottom.
        let mut top = frame.height - PADDING;
        top -= title_height;
        self.title.setFrame(rect(PADDING, top, inner, title_height));
        top -= TITLE_GAP;
        let value_x = PADDING + label_width + COLUMN_GAP;
        for ((label, value), row) in self.facts.iter().zip(&rows) {
            top -= row;
            label.setFrame(rect(PADDING, top, label_width, *row));
            value.setFrame(rect(value_x, top, column, *row));
            top -= ROW_GAP;
        }
        top -= path_height;
        // The label sits on the first line of the path.
        self.path_label.setFrame(rect(
            PADDING,
            top + path_height - path_label_height,
            label_width,
            path_label_height,
        ));
        self.path.setFrame(rect(value_x, top, column, path_height));
        Some(frame)
    }

    /// Shows the panel at `frame`, as laid out by the last
    /// [`InfoPanel::prepare`].
    pub(super) fn show(&self, frame: ScreenRect) {
        self.overlay.show(self.mtm, frame);
    }

    /// Hides the panel. Does nothing when it is hidden.
    pub(super) fn hide(&self) {
        self.overlay.hide();
    }

    /// Puts `path` in its field, cut in the middle if it takes more than
    /// [`PATH_LINES`] lines at `column` points, and returns its height.
    fn fit_path(&self, path: &str, column: f64) -> f64 {
        self.path.setPreferredMaxLayoutWidth(column);
        let measure = |text: &str| {
            self.path.setStringValue(&NSString::from_str(text));
            height_of(&self.path, column)
        };
        let limit = measure(&["W"; PATH_LINES].join("\n"));
        let whole = measure(path);
        if whole <= limit {
            return whole;
        }
        // The longest cut that fits: a cut of `keep` characters fits for
        // every `keep` below some bound, and `keep = 0` (the ellipsis
        // alone) always does.
        let (mut fits, mut fails) = (0, path.chars().count());
        while fails - fits > 1 {
            let keep = fits + (fails - fits) / 2;
            if measure(&middle_cut(path, keep)) <= limit {
                fits = keep;
            } else {
                fails = keep;
            }
        }
        measure(&middle_cut(path, fits))
    }
}

/// A grey label of a fact, aligned right.
fn label_field(mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    field.setFont(Some(&NSFont::systemFontOfSize(FONT_SIZE)));
    field.setTextColor(Some(&NSColor::secondaryLabelColor()));
    field.setAlignment(NSTextAlignment::Right);
    field.setMaximumNumberOfLines(1);
    field
}

/// The value of a fact: one line, cut at the end.
fn value_field(mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    field.setFont(Some(&NSFont::systemFontOfSize(FONT_SIZE)));
    field.setTextColor(Some(&NSColor::labelColor()));
    field.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    field.setMaximumNumberOfLines(1);
    field
}

/// The height `field` needs at `width` points, whole points up.
fn height_of(field: &NSTextField, width: f64) -> f64 {
    field
        .sizeThatFits(NSSize::new(width, UNBOUNDED))
        .height
        .ceil()
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

/// `text` with its middle replaced by "…", keeping `keep` characters: half
/// from the start and half from the end, the end getting the odd one.
/// `text` itself when it is not longer than `keep`.
fn middle_cut(text: &str, keep: usize) -> String {
    let count = text.chars().count();
    if count <= keep {
        return text.to_owned();
    }
    let head = keep / 2;
    let tail = keep - head;
    let mut cut: String = text.chars().take(head).collect();
    cut.push('…');
    cut.extend(text.chars().skip(count - tail));
    cut
}

#[cfg(test)]
mod tests {
    use super::middle_cut;

    #[test]
    fn the_middle_goes_and_the_end_keeps_the_odd_character() {
        assert_eq!(middle_cut("/abcdefgh/name", 5), "/a…ame");
        assert_eq!(middle_cut("/abcdefgh/name", 4), "/a…me");
    }

    #[test]
    fn characters_are_cut_whole() {
        assert_eq!(middle_cut("/Документы/отчёт", 6), "/До…чёт");
    }

    #[test]
    fn a_text_that_fits_stays() {
        assert_eq!(middle_cut("/tmp/a", 6), "/tmp/a");
        assert_eq!(middle_cut("/tmp/a", 100), "/tmp/a");
    }

    #[test]
    fn keeping_nothing_leaves_the_ellipsis() {
        assert_eq!(middle_cut("/tmp/a", 0), "…");
    }
}
