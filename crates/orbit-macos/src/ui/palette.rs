//! Colors of the radial menu.
//!
//! Pure: no AppKit and no Core Graphics. The view turns an [`Rgba`] into a
//! layer color at the last moment, and reads the dynamic system colors
//! (accent, red) into [`Rgba`] when it builds a [`Palette`], which is on
//! every show.

use orbit_domain::action::CANCEL_SLOT;

/// Alpha of the highlight fill.
const HIGHLIGHT_ALPHA: f64 = 0.85;
/// Minimum contrast of an icon against its sector (WCAG 1.4.11, graphics).
const MIN_ICON_CONTRAST: f64 = 3.0;
/// Alpha of the icon ink on a highlighted sector.
const HIGHLIGHT_INK_ALPHA: f64 = 0.95;

/// A color in sRGB with alpha, every component in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

impl Rgba {
    /// A color from its components.
    pub const fn new(r: f64, g: f64, b: f64, a: f64) -> Self {
        Self { r, g, b, a }
    }

    /// The same color with another alpha.
    pub const fn with_alpha(self, a: f64) -> Self {
        Self { a, ..self }
    }
}

/// Color of an icon: white or black.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    White,
    Black,
}

impl Ink {
    /// The ink with `alpha`.
    pub const fn color(self, alpha: f64) -> Rgba {
        match self {
            Self::White => Rgba::new(1.0, 1.0, 1.0, alpha),
            Self::Black => Rgba::new(0.0, 0.0, 0.0, alpha),
        }
    }
}

/// Relative luminance of an sRGB color (WCAG 2).
pub fn luminance(r: f64, g: f64, b: f64) -> f64 {
    let linear = |c: f64| {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// The ink for an icon on a fill: white while it keeps the minimum contrast,
/// black otherwise. The user picks the accent, so a yellow or orange fill
/// must not get a white icon. The fill is treated as opaque; its alpha shifts
/// the real color a little, enough to matter only near the threshold.
pub fn ink_for_fill(r: f64, g: f64, b: f64) -> Ink {
    let white_contrast = 1.05 / (luminance(r, g, b) + 0.05);
    if white_contrast >= MIN_ICON_CONTRAST {
        Ink::White
    } else {
        Ink::Black
    }
}

/// Whether the surface under the menu is dark or light.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

/// A highlight fill and the ink that reads on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accent {
    pub fill: Rgba,
    pub ink: Ink,
}

impl Accent {
    /// The highlight made from an opaque `base` color.
    pub fn from_base(base: Rgba) -> Self {
        Self {
            fill: base.with_alpha(HIGHLIGHT_ALPHA),
            ink: ink_for_fill(base.r, base.g, base.b),
        }
    }
}

/// What a slot shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotState {
    /// The cursor is over the slot.
    pub highlighted: bool,
    /// The action can be chosen.
    pub available: bool,
}

/// Colors of the menu for one theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    theme: Theme,
    accent: Accent,
    cancel: Accent,
}

impl Palette {
    /// A palette for `theme`; `accent` and `cancel` are the opaque base
    /// colors of the highlight of an ordinary slot and of the cancel slot.
    pub fn new(theme: Theme, accent: Rgba, cancel: Rgba) -> Self {
        Self {
            theme,
            accent: Accent::from_base(accent),
            cancel: Accent::from_base(cancel),
        }
    }

    /// The theme the palette was made for.
    pub const fn theme(&self) -> Theme {
        self.theme
    }

    /// Opaque disc under the sectors, for Reduce Transparency and the plain
    /// background.
    pub const fn disc(&self) -> Rgba {
        match self.theme {
            Theme::Dark => Rgba::new(0.12, 0.12, 0.14, 0.96),
            Theme::Light => Rgba::new(0.96, 0.96, 0.97, 0.96),
        }
    }

    const fn highlight(&self, slot: usize) -> &Accent {
        if slot == CANCEL_SLOT {
            &self.cancel
        } else {
            &self.accent
        }
    }

    /// Fill of the sector `slot`.
    pub const fn sector(&self, slot: usize, state: SlotState) -> Rgba {
        let dark = matches!(self.theme, Theme::Dark);
        match (state.highlighted, state.available) {
            (true, true) => self.highlight(slot).fill,
            // The cursor is over the slot but it cannot be chosen.
            (true, false) => Rgba::new(0.5, 0.5, 0.5, 0.25),
            (false, true) if slot == CANCEL_SLOT => Rgba::new(0.55, 0.15, 0.15, 0.30),
            (false, true) if dark => Rgba::new(1.0, 1.0, 1.0, 0.10),
            (false, true) => Rgba::new(0.0, 0.0, 0.0, 0.08),
            (false, false) if dark => Rgba::new(1.0, 1.0, 1.0, 0.04),
            (false, false) => Rgba::new(0.0, 0.0, 0.0, 0.03),
        }
    }

    /// Color of the icon of `slot`.
    pub const fn icon(&self, slot: usize, state: SlotState) -> Rgba {
        if state.highlighted && state.available {
            return self.highlight(slot).ink.color(HIGHLIGHT_INK_ALPHA);
        }
        match (self.theme, state.available) {
            (Theme::Dark, true) => Ink::White.color(0.95),
            (Theme::Dark, false) => Ink::White.color(0.30),
            (Theme::Light, true) => Ink::Black.color(0.80),
            (Theme::Light, false) => Ink::Black.color(0.30),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLUE: Rgba = Rgba::new(0.0, 0.478, 1.0, 1.0);
    const RED: Rgba = Rgba::new(1.0, 0.231, 0.188, 1.0);
    const IDLE: SlotState = SlotState {
        highlighted: false,
        available: true,
    };
    const UNAVAILABLE: SlotState = SlotState {
        highlighted: false,
        available: false,
    };
    const HOVERED: SlotState = SlotState {
        highlighted: true,
        available: true,
    };
    const HOVERED_UNAVAILABLE: SlotState = SlotState {
        highlighted: true,
        available: false,
    };

    fn palette(theme: Theme) -> Palette {
        Palette::new(theme, BLUE, RED)
    }

    #[test]
    fn light_accents_get_black_icons() {
        // systemYellow and systemOrange: white would fall under 3:1.
        assert_eq!(ink_for_fill(1.0, 0.8, 0.0), Ink::Black);
        assert_eq!(ink_for_fill(1.0, 0.584, 0.0), Ink::Black);
        assert_eq!(ink_for_fill(1.0, 1.0, 1.0), Ink::Black);
    }

    #[test]
    fn dark_accents_keep_white_icons() {
        // systemBlue, pure red and black.
        assert_eq!(ink_for_fill(0.0, 0.478, 1.0), Ink::White);
        assert_eq!(ink_for_fill(1.0, 0.0, 0.0), Ink::White);
        assert_eq!(ink_for_fill(0.0, 0.0, 0.0), Ink::White);
    }

    #[test]
    fn luminance_runs_from_black_to_white() {
        assert!(luminance(0.0, 0.0, 0.0).abs() < 1e-12);
        assert!((luminance(1.0, 1.0, 1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn highlight_keeps_the_base_color_with_the_highlight_alpha() {
        let accent = Accent::from_base(BLUE);
        assert_eq!(accent.fill, BLUE.with_alpha(HIGHLIGHT_ALPHA));
        assert_eq!(accent.ink, Ink::White);
    }

    #[test]
    fn idle_sector_follows_the_theme() {
        assert_eq!(
            palette(Theme::Dark).sector(0, IDLE),
            Rgba::new(1.0, 1.0, 1.0, 0.10)
        );
        assert_eq!(
            palette(Theme::Light).sector(0, IDLE),
            Rgba::new(0.0, 0.0, 0.0, 0.08)
        );
    }

    #[test]
    fn unavailable_sector_is_fainter_than_an_idle_one() {
        for theme in [Theme::Dark, Theme::Light] {
            let palette = palette(theme);
            assert!(palette.sector(0, UNAVAILABLE).a < palette.sector(0, IDLE).a);
            assert!(palette.icon(0, UNAVAILABLE).a < palette.icon(0, IDLE).a);
        }
    }

    #[test]
    fn hovered_sector_uses_the_accent_and_the_cancel_slot_uses_red() {
        let palette = palette(Theme::Dark);
        assert_eq!(palette.sector(0, HOVERED), BLUE.with_alpha(HIGHLIGHT_ALPHA));
        assert_eq!(
            palette.sector(CANCEL_SLOT, HOVERED),
            RED.with_alpha(HIGHLIGHT_ALPHA)
        );
        // At rest the cancel slot is already tinted.
        assert_eq!(
            palette.sector(CANCEL_SLOT, IDLE),
            Rgba::new(0.55, 0.15, 0.15, 0.30)
        );
    }

    #[test]
    fn hovered_unavailable_sector_is_dimmed_not_highlighted() {
        for theme in [Theme::Dark, Theme::Light] {
            let palette = palette(theme);
            assert_eq!(
                palette.sector(0, HOVERED_UNAVAILABLE),
                Rgba::new(0.5, 0.5, 0.5, 0.25)
            );
            // The icon stays as faint as for any unavailable slot.
            assert_eq!(
                palette.icon(0, HOVERED_UNAVAILABLE),
                palette.icon(0, UNAVAILABLE)
            );
        }
    }

    #[test]
    fn icon_on_a_light_accent_is_black() {
        let yellow = Rgba::new(1.0, 0.8, 0.0, 1.0);
        let palette = Palette::new(Theme::Dark, yellow, RED);
        assert_eq!(palette.icon(0, HOVERED), Ink::Black.color(0.95));
        assert_eq!(palette.icon(0, IDLE), Ink::White.color(0.95));
    }

    #[test]
    fn idle_icons_follow_the_theme() {
        assert_eq!(palette(Theme::Dark).icon(0, IDLE), Ink::White.color(0.95));
        assert_eq!(palette(Theme::Light).icon(0, IDLE), Ink::Black.color(0.80));
    }

    #[test]
    fn disc_is_nearly_opaque_in_both_themes() {
        assert_eq!(
            palette(Theme::Dark).disc(),
            Rgba::new(0.12, 0.12, 0.14, 0.96)
        );
        assert_eq!(
            palette(Theme::Light).disc(),
            Rgba::new(0.96, 0.96, 0.97, 0.96)
        );
    }
}
