//! The toast: one line next to the pointer that goes away by itself.
//!
//! There is one window for the life of the application; a new toast
//! replaces the one on screen. One `NSTimer` hides it: a new toast
//! invalidates the timer of the old one before it schedules its own, so a
//! long timer of an error never hides a later, shorter toast early. The
//! timer exists only while a toast is on screen.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::MainThreadBound;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSColor, NSEvent, NSFont, NSImage, NSImageView, NSLineBreakMode, NSTextField};
use objc2_foundation::{
    NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use orbit_domain::geometry::ScreenPoint;
use tracing::{debug, warn};

use super::backdrop::Background;
use super::notice_text::{ToastText, Tone};
use super::overlay::{Overlay, SCREEN_MARGIN};
use super::placement::place_panel;
use crate::platform::coordinates::Displays;

const HEIGHT: f64 = 40.0;
const ICON: f64 = 18.0;
const PADDING_LEFT: f64 = 14.0;
const PADDING_RIGHT: f64 = 16.0;
const SPACING: f64 = 8.0;
/// Longest text before it is cut with an ellipsis, in points.
const MAX_TEXT_WIDTH: f64 = 420.0;
const FONT_SIZE: f64 = 13.0;

/// What the window and the timer must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Invalidate the timer of the toast on screen.
    CancelTimer,
    /// Schedule the timer that hides this toast.
    Schedule(Duration),
    /// Take the toast off the screen.
    Hide,
}

/// Whether a toast is on screen, and so whether a timer runs.
#[derive(Debug, Default)]
struct ToastState {
    shown: bool,
}

impl ToastState {
    /// A toast that stays for `duration` replaces any toast on screen.
    fn show(&mut self, duration: Duration) -> Vec<Step> {
        let mut steps = Vec::with_capacity(2);
        if self.shown {
            steps.push(Step::CancelTimer);
        }
        steps.push(Step::Schedule(duration));
        self.shown = true;
        steps
    }

    /// The timer fired. A timer that fires with no toast on screen was
    /// already invalidated and has nothing to hide.
    fn fired(&mut self) -> Vec<Step> {
        if self.shown {
            self.shown = false;
            vec![Step::Hide]
        } else {
            Vec::new()
        }
    }
}

/// The toast window.
pub struct Toast {
    inner: Rc<RefCell<Inner>>,
}

struct Inner {
    mtm: MainThreadMarker,
    overlay: Overlay,
    icon: Retained<NSImageView>,
    label: Retained<NSTextField>,
    state: ToastState,
    timer: Option<Retained<NSTimer>>,
}

impl Toast {
    /// A hidden toast window with `background`.
    pub fn new(mtm: MainThreadMarker, background: Background) -> Self {
        let overlay = Overlay::new(mtm, background, HEIGHT / 2.0);
        let icon = NSImageView::initWithFrame(
            NSImageView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(ICON, ICON)),
        );
        let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        label.setFont(Some(&NSFont::systemFontOfSize(FONT_SIZE)));
        label.setTextColor(Some(&NSColor::labelColor()));
        label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        label.setMaximumNumberOfLines(1);
        overlay.content().addSubview(&icon);
        overlay.content().addSubview(&label);
        Self {
            inner: Rc::new(RefCell::new(Inner {
                mtm,
                overlay,
                icon,
                label,
                state: ToastState::default(),
                timer: None,
            })),
        }
    }

    /// Shows `text` next to the pointer, replacing any toast on screen.
    pub fn show(&self, text: &ToastText) {
        let weak = Rc::downgrade(&self.inner);
        match self.inner.try_borrow_mut() {
            Ok(mut inner) => inner.show(text, weak),
            Err(_) => warn!(text = %text.text, "the toast is busy; the toast is dropped"),
        }
    }
}

impl Inner {
    fn show(&mut self, text: &ToastText, weak: Weak<RefCell<Self>>) {
        let started = Instant::now();
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(text.symbol),
            None,
        );
        if image.is_none() {
            warn!(
                symbol = text.symbol,
                "no such SF Symbol; the toast has no icon"
            );
        }
        self.icon.setImage(image.as_deref());
        let tint = match text.tone {
            Tone::Success => NSColor::systemGreenColor(),
            Tone::Failure => NSColor::systemOrangeColor(),
            Tone::Neutral => NSColor::secondaryLabelColor(),
        };
        self.icon.setContentTintColor(Some(&tint));
        self.label.setStringValue(&NSString::from_str(&text.text));

        let fitting = self.label.fittingSize();
        let text_width = fitting.width.ceil().min(MAX_TEXT_WIDTH);
        let wanted = (
            PADDING_LEFT + ICON + SPACING + text_width + PADDING_RIGHT,
            HEIGHT,
        );
        let anchor = NSEvent::mouseLocation();
        let anchor = ScreenPoint::new(anchor.x, anchor.y);
        let screens = Displays::current(self.mtm)
            .map(|d| d.screens)
            .unwrap_or_default();
        let Some(frame) = place_panel(anchor, wanted, &screens, SCREEN_MARGIN) else {
            warn!(text = %text.text, "no display; the toast is not shown");
            return;
        };
        self.icon.setFrame(NSRect::new(
            NSPoint::new(PADDING_LEFT, ((frame.height - ICON) / 2.0).round()),
            NSSize::new(ICON, ICON),
        ));
        let label_x = PADDING_LEFT + ICON + SPACING;
        let label_width = (frame.width - label_x - PADDING_RIGHT).max(0.0);
        self.label.setFrame(NSRect::new(
            NSPoint::new(label_x, ((frame.height - fitting.height) / 2.0).round()),
            NSSize::new(label_width, fitting.height),
        ));
        self.overlay.show(self.mtm, frame);

        for step in self.state.show(text.duration) {
            self.run(step, &weak);
        }
        debug!(elapsed = ?started.elapsed(), text = %text.text, "toast shown");
    }

    fn fired(&mut self) {
        let weak = Weak::new();
        for step in self.state.fired() {
            self.run(step, &weak);
        }
    }

    fn run(&mut self, step: Step, weak: &Weak<RefCell<Self>>) {
        match step {
            Step::CancelTimer => {
                if let Some(timer) = self.timer.take() {
                    timer.invalidate();
                }
            }
            Step::Schedule(duration) => {
                self.timer = Some(schedule(self.mtm, duration, weak.clone()));
            }
            Step::Hide => {
                // The timer that fired is done; it invalidated itself.
                self.timer = None;
                self.overlay.hide();
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.take() {
            timer.invalidate();
        }
    }
}

/// A timer on the main run loop that tells `toast` it fired after
/// `duration`. It runs in the common modes, so it also fires while a menu
/// of the status item is tracking.
fn schedule(
    mtm: MainThreadMarker,
    duration: Duration,
    toast: Weak<RefCell<Inner>>,
) -> Retained<NSTimer> {
    let toast = MainThreadBound::new(toast, mtm);
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let Some(mtm) = MainThreadMarker::new() else {
            warn!("the toast timer fired off the main thread");
            return;
        };
        let Some(toast) = toast.get(mtm).upgrade() else {
            return;
        };
        match toast.try_borrow_mut() {
            Ok(mut toast) => toast.fired(),
            Err(_) => warn!("the toast is busy when its timer fired"),
        }
    });
    // SAFETY: the block is `Send` and `Sync`: its only capture is a
    // `MainThreadBound`, and it reaches the toast only on the main thread.
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_repeats_block(duration.as_secs_f64(), false, &block)
    };
    // SAFETY: the timer is new and not scheduled anywhere; the mode is a
    // Foundation constant. The main run loop fires it on the main thread.
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    timer
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(4);
    const SHORT: Duration = Duration::from_millis(1500);

    #[test]
    fn a_toast_schedules_its_timer_and_the_timer_hides_it() {
        let mut state = ToastState::default();
        assert_eq!(state.show(SHORT), vec![Step::Schedule(SHORT)]);
        assert_eq!(state.fired(), vec![Step::Hide]);
        assert!(!state.shown, "no timer after the toast is hidden");
    }

    #[test]
    fn a_new_toast_cancels_the_timer_of_the_old_one() {
        let mut state = ToastState::default();
        assert_eq!(state.show(LONG), vec![Step::Schedule(LONG)]);
        // The error is replaced by good news before its four seconds end.
        assert_eq!(
            state.show(SHORT),
            vec![Step::CancelTimer, Step::Schedule(SHORT)]
        );
        // Only the short timer is left, and it hides the toast.
        assert_eq!(state.fired(), vec![Step::Hide]);
        assert!(!state.shown);
    }

    #[test]
    fn with_no_toast_on_screen_a_firing_does_nothing() {
        let mut state = ToastState::default();
        assert_eq!(state.fired(), vec![]);
        state.show(SHORT);
        state.fired();
        assert_eq!(state.fired(), vec![]);
    }
}
