//! What the radial menu view keeps between calls of [`MenuView`].
//!
//! The view does not know whether a call matters: this state decides. Pure,
//! so the rules of the port contract are tested without a window.
//!
//! [`MenuView`]: orbit_app::ports::MenuView

use orbit_app::state::Highlight;

/// What a [`Session::highlight`] call asks the view to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightChange {
    /// No menu is open; the call is ignored.
    Closed,
    /// The highlight is already this one; nothing to repaint.
    Unchanged,
    /// The slot index is outside the menu; the highlight stays as it was.
    OutOfRange,
    /// Repaint the slot of `from` (if any) and the slot of `to` (if any).
    Changed { from: Highlight, to: Highlight },
}

/// Whether a menu is open and what it highlights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Session {
    open: bool,
    highlight: Highlight,
}

impl Session {
    /// A session with no menu open.
    pub const fn new() -> Self {
        Self {
            open: false,
            highlight: Highlight::None,
        }
    }

    /// A menu is shown, replacing the open one if there is one. The
    /// highlight starts over.
    pub fn present(&mut self) {
        self.open = true;
        self.highlight = Highlight::None;
    }

    /// Moves the highlight of the open menu among `slots` slots.
    pub fn highlight(&mut self, to: Highlight, slots: usize) -> HighlightChange {
        if !self.open {
            return HighlightChange::Closed;
        }
        if let Highlight::Active(slot) | Highlight::Inactive(slot) = to
            && slot >= slots
        {
            return HighlightChange::OutOfRange;
        }
        if self.highlight == to {
            return HighlightChange::Unchanged;
        }
        let from = std::mem::replace(&mut self.highlight, to);
        HighlightChange::Changed { from, to }
    }

    /// Closes the menu. Returns whether one was open.
    pub fn hide(&mut self) -> bool {
        let was_open = self.open;
        *self = Self::new();
        was_open
    }

    /// The view refuses to show a model it cannot draw. This closes the
    /// session just like [`hide`](Self::hide), so a menu that was open is not
    /// left on screen with the picture of the previous model. Returns whether
    /// one was open.
    pub fn refuse(&mut self) -> bool {
        self.hide()
    }

    /// Whether a menu is open.
    #[cfg(test)]
    const fn is_open(&self) -> bool {
        self.open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOTS: usize = 9;

    fn open() -> Session {
        let mut session = Session::new();
        session.present();
        session
    }

    #[test]
    fn highlight_before_present_is_ignored() {
        let mut session = Session::new();
        assert_eq!(
            session.highlight(Highlight::Active(1), SLOTS),
            HighlightChange::Closed
        );
        assert!(!session.is_open());
    }

    #[test]
    fn highlight_moves_and_repeats_are_free() {
        let mut session = open();
        assert_eq!(
            session.highlight(Highlight::Active(1), SLOTS),
            HighlightChange::Changed {
                from: Highlight::None,
                to: Highlight::Active(1)
            }
        );
        assert_eq!(
            session.highlight(Highlight::Active(1), SLOTS),
            HighlightChange::Unchanged
        );
        assert_eq!(
            session.highlight(Highlight::Inactive(1), SLOTS),
            HighlightChange::Changed {
                from: Highlight::Active(1),
                to: Highlight::Inactive(1)
            }
        );
        assert_eq!(
            session.highlight(Highlight::None, SLOTS),
            HighlightChange::Changed {
                from: Highlight::Inactive(1),
                to: Highlight::None
            }
        );
    }

    #[test]
    fn slot_outside_the_menu_changes_nothing() {
        let mut session = open();
        session.highlight(Highlight::Active(2), SLOTS);
        for bad in [Highlight::Active(SLOTS), Highlight::Inactive(usize::MAX)] {
            assert_eq!(session.highlight(bad, SLOTS), HighlightChange::OutOfRange);
        }
        // The highlight is still slot 2.
        assert_eq!(
            session.highlight(Highlight::Active(2), SLOTS),
            HighlightChange::Unchanged
        );
    }

    #[test]
    fn present_over_an_open_menu_starts_the_highlight_over() {
        let mut session = open();
        session.highlight(Highlight::Active(3), SLOTS);
        session.present();
        assert!(session.is_open());
        assert_eq!(
            session.highlight(Highlight::Active(3), SLOTS),
            HighlightChange::Changed {
                from: Highlight::None,
                to: Highlight::Active(3)
            }
        );
    }

    #[test]
    fn hide_reports_whether_a_menu_was_open() {
        let mut session = open();
        assert!(session.hide());
        assert!(!session.hide());
        session.present();
        assert!(session.is_open());
    }

    #[test]
    fn refusal_closes_an_open_menu() {
        let mut session = open();
        assert_eq!(
            session.highlight(Highlight::Active(1), SLOTS),
            HighlightChange::Changed {
                from: Highlight::None,
                to: Highlight::Active(1)
            }
        );
        // The next model cannot be drawn.
        assert!(session.refuse());
        assert!(!session.is_open());
        assert_eq!(
            session.highlight(Highlight::Active(2), SLOTS),
            HighlightChange::Closed
        );
        assert!(!session.hide());
        // A model that can be drawn opens it again, from no highlight.
        session.present();
        assert_eq!(
            session.highlight(Highlight::Active(1), SLOTS),
            HighlightChange::Changed {
                from: Highlight::None,
                to: Highlight::Active(1)
            }
        );
    }

    #[test]
    fn refusal_with_nothing_open_reports_it() {
        let mut session = Session::new();
        assert!(!session.refuse());
    }
}
