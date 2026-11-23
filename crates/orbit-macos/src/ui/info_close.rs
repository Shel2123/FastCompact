//! When the info panel closes: on a click, a key, or a newly pressed
//! modifier anywhere, while Finder stays the active application.
//!
//! Two `NSEvent` monitors watch while the panel is up and only then: a
//! global one for the events of other applications and a local one for
//! the events sent to Orbit itself (its item in the menu bar), which a
//! global monitor does not see. The local monitor passes every event on
//! unchanged. The watch starts only with both: with one missing the panel
//! would stay up after a click that should close it.
//!
//! Releasing a modifier never closes the panel: the panel appears when the
//! gesture ends, often while the other modifiers of the hotkey are still
//! held, and letting them go must not take it away. [`CloseRule`] keeps
//! the modifiers last seen and closes only on one that was not down.

use std::cell::RefCell;
use std::fmt;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};

use block2::RcBlock;
use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
use tracing::{info, warn};

/// The modifiers that count. Caps Lock and Fn are left out: they are
/// toggles, and a change of them alone does not close the panel.
const MODIFIERS: NSEventModifierFlags = NSEventModifierFlags::Shift
    .union(NSEventModifierFlags::Control)
    .union(NSEventModifierFlags::Option)
    .union(NSEventModifierFlags::Command);

/// The events the monitors ask for.
const WATCHED: NSEventMask = NSEventMask::LeftMouseDown
    .union(NSEventMask::RightMouseDown)
    .union(NSEventMask::OtherMouseDown)
    .union(NSEventMask::KeyDown)
    .union(NSEventMask::FlagsChanged);

/// An event that may close the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Input {
    MouseDown,
    KeyDown,
    /// The modifiers now down.
    FlagsChanged(NSEventModifierFlags),
}

impl Input {
    /// The input `event` is, if it is one the panel cares about.
    fn from_event(event: &NSEvent) -> Option<Self> {
        let kind = event.r#type();
        if kind == NSEventType::LeftMouseDown
            || kind == NSEventType::RightMouseDown
            || kind == NSEventType::OtherMouseDown
        {
            Some(Self::MouseDown)
        } else if kind == NSEventType::KeyDown {
            Some(Self::KeyDown)
        } else if kind == NSEventType::FlagsChanged {
            Some(Self::FlagsChanged(event.modifierFlags()))
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::MouseDown => "MouseDown",
            Self::KeyDown => "KeyDown",
            Self::FlagsChanged(_) => "FlagsChanged",
        }
    }
}

/// Decides, event by event, whether the panel closes.
#[derive(Debug)]
struct CloseRule {
    /// The modifiers down after the last event, of [`MODIFIERS`] only.
    prev: NSEventModifierFlags,
}

impl CloseRule {
    /// A rule for a panel shown while `current` modifiers are down.
    fn new(current: NSEventModifierFlags) -> Self {
        Self {
            prev: current & MODIFIERS,
        }
    }

    /// Whether `input` closes the panel. A click or a key always does; a
    /// change of modifiers does only when a modifier that was up is now
    /// down. Every change of modifiers is remembered, closing or not, so a
    /// modifier let go and pressed again closes on the press.
    fn on(&mut self, input: Input) -> bool {
        match input {
            Input::MouseDown | Input::KeyDown => true,
            Input::FlagsChanged(now) => {
                let now = now & MODIFIERS;
                let pressed = now.difference(self.prev);
                self.prev = now;
                !pressed.is_empty()
            }
        }
    }
}

/// Modifiers as words for the log.
struct Mods(NSEventModifierFlags);

impl fmt::Display for Mods {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = [
            (NSEventModifierFlags::Shift, "shift"),
            (NSEventModifierFlags::Control, "ctrl"),
            (NSEventModifierFlags::Option, "option"),
            (NSEventModifierFlags::Command, "cmd"),
        ];
        let mut first = true;
        for (flag, name) in names {
            if self.0.contains(flag) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        if first {
            f.write_str("none")?;
        }
        Ok(())
    }
}

/// What the monitors share.
struct Shared {
    rule: CloseRule,
    /// What to do on closing; `None` once closed or stopped, and then the
    /// monitors stay silent until they are removed.
    on_close: Option<Box<dyn FnOnce()>>,
    monitors: Vec<Retained<AnyObject>>,
}

/// Watches for what closes the panel from [`CloseWatch::start`] until the
/// panel closes or the watch is dropped.
pub(super) struct CloseWatch {
    mtm: MainThreadMarker,
    shared: Rc<RefCell<Shared>>,
}

impl CloseWatch {
    /// Starts watching; `on_close` runs once, on the first event that
    /// closes the panel. `None` when either monitor cannot be added: the one
    /// that was added is removed again and `on_close` never runs.
    pub(super) fn start(mtm: MainThreadMarker, on_close: impl FnOnce() + 'static) -> Option<Self> {
        let shared = Rc::new(RefCell::new(Shared {
            rule: CloseRule::new(NSEventModifierFlags::empty()),
            on_close: Some(Box::new(on_close)),
            monitors: Vec::with_capacity(2),
        }));

        let global = {
            let weak = MainThreadBound::new(Rc::downgrade(&shared), mtm);
            let block = RcBlock::new(move |event: NonNull<NSEvent>| {
                handle(&weak, event, "global");
            });
            NSEvent::addGlobalMonitorForEventsMatchingMask_handler(WATCHED, &block)
        };
        let local = {
            let weak = MainThreadBound::new(Rc::downgrade(&shared), mtm);
            let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
                handle(&weak, event, "local");
                event.as_ptr()
            });
            // SAFETY: the block returns the event it was given, a valid
            // pointer, so AppKit dispatches it as if there were no monitor.
            unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(WATCHED, &block) }
        };
        if global.is_none() {
            warn!("no global event monitor; the info panel would not close on other applications");
        }
        if local.is_none() {
            warn!("no local event monitor; the info panel would not close on clicks at Orbit");
        }
        let (global, local) = match (global, local) {
            (Some(global), Some(local)) => (global, local),
            (global, local) => {
                let added: Vec<_> = global.into_iter().chain(local).collect();
                remove_later(mtm, added);
                return None;
            }
        };

        // The modifiers are read after the monitors are in place. Handlers
        // run on the main thread from the run loop, so none runs before
        // this read: a change before it shows in `current`, a change after
        // it arrives as an event, and no stale modifier is left in the rule.
        let current = NSEvent::modifierFlags_class();
        let installed = {
            let mut shared = shared.borrow_mut();
            shared.rule = CloseRule::new(current);
            shared.monitors.push(global);
            shared.monitors.push(local);
            shared.monitors.len()
        };
        info!(
            installed,
            flags = %Mods(current & MODIFIERS),
            "info watch started"
        );
        Some(Self { mtm, shared })
    }
}

impl Drop for CloseWatch {
    fn drop(&mut self) {
        let monitors = match self.shared.try_borrow_mut() {
            Ok(mut shared) => {
                shared.on_close = None;
                std::mem::take(&mut shared.monitors)
            }
            Err(_) => {
                warn!("the info watch is busy when dropped; its monitors stay");
                return;
            }
        };
        remove_later(self.mtm, monitors);
    }
}

/// One event from a monitor named `source`.
fn handle(
    weak: &MainThreadBound<Weak<RefCell<Shared>>>,
    event: NonNull<NSEvent>,
    source: &'static str,
) {
    let Some(mtm) = MainThreadMarker::new() else {
        warn!(source, "an info monitor ran off the main thread");
        return;
    };
    let Some(shared) = weak.get(mtm).upgrade() else {
        return;
    };
    // SAFETY: AppKit hands the monitor a valid event for the duration of
    // the call.
    let event = unsafe { event.as_ref() };
    let Some(input) = Input::from_event(event) else {
        return;
    };
    let (on_close, monitors) = {
        let Ok(mut shared) = shared.try_borrow_mut() else {
            warn!(source, "the info watch is busy; the event is not checked");
            return;
        };
        if shared.on_close.is_none() {
            // Closed already; the monitors are on their way out.
            return;
        }
        let flags = match input {
            Input::FlagsChanged(now) => Some(Mods(now & MODIFIERS)),
            Input::MouseDown | Input::KeyDown => None,
        };
        let flags = flags.map(|f| f.to_string()).unwrap_or_default();
        if !shared.rule.on(input) {
            info!(input = input.name(), source, flags, "info ignores");
            return;
        }
        info!(input = input.name(), source, flags, "info closes");
        (shared.on_close.take(), std::mem::take(&mut shared.monitors))
    };
    remove_later(mtm, monitors);
    if let Some(on_close) = on_close {
        on_close();
    }
}

/// Removes `monitors` on a later turn of the main queue: never from inside
/// a handler of the monitor being removed.
fn remove_later(mtm: MainThreadMarker, monitors: Vec<Retained<AnyObject>>) {
    if monitors.is_empty() {
        return;
    }
    let monitors = MainThreadBound::new(monitors, mtm);
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            warn!("the info monitors are removed off the main thread");
            return;
        };
        let monitors = monitors.into_inner(mtm);
        for monitor in &monitors {
            // SAFETY: every object here was returned by `add…Monitor…`, and
            // each is removed once: it was taken out of the watch before.
            unsafe { NSEvent::removeMonitor(monitor) };
        }
        info!(removed = monitors.len(), "info monitors removed");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIFT: NSEventModifierFlags = NSEventModifierFlags::Shift;
    const CTRL: NSEventModifierFlags = NSEventModifierFlags::Control;
    const OPTION: NSEventModifierFlags = NSEventModifierFlags::Option;
    const NONE: NSEventModifierFlags = NSEventModifierFlags::empty();

    fn flags(rule: &mut CloseRule, steps: &[NSEventModifierFlags]) -> Vec<bool> {
        steps
            .iter()
            .map(|&now| rule.on(Input::FlagsChanged(now)))
            .collect()
    }

    #[test]
    fn letting_go_of_the_hotkey_keeps_the_panel_and_a_new_press_closes_it() {
        let mut rule = CloseRule::new(CTRL | OPTION);
        assert_eq!(
            flags(&mut rule, &[CTRL, NONE, CTRL]),
            vec![false, false, true]
        );
    }

    #[test]
    fn a_modifier_pressed_after_the_panel_appears_closes_it() {
        let mut rule = CloseRule::new(NONE);
        assert!(rule.on(Input::FlagsChanged(OPTION)));
    }

    #[test]
    fn one_event_that_releases_one_modifier_and_presses_another_closes() {
        let mut rule = CloseRule::new(SHIFT | CTRL);
        assert!(rule.on(Input::FlagsChanged(CTRL | OPTION)));
    }

    #[test]
    fn a_key_or_a_click_closes() {
        assert!(CloseRule::new(CTRL).on(Input::KeyDown));
        assert!(CloseRule::new(CTRL).on(Input::MouseDown));
    }

    #[test]
    fn caps_lock_and_fn_do_not_close() {
        let mut rule = CloseRule::new(NONE);
        assert_eq!(
            flags(
                &mut rule,
                &[
                    NSEventModifierFlags::CapsLock,
                    NSEventModifierFlags::Function,
                ]
            ),
            vec![false, false]
        );
    }

    #[test]
    fn a_modifier_let_go_and_pressed_again_closes_on_the_press() {
        let mut rule = CloseRule::new(CTRL);
        assert_eq!(flags(&mut rule, &[NONE, CTRL]), vec![false, true]);
    }

    #[test]
    fn modifiers_read_as_words() {
        assert_eq!(Mods(CTRL | OPTION).to_string(), "ctrl|option");
        assert_eq!(Mods(NONE).to_string(), "none");
    }
}
