//! Design tokens: the only sizes the shell draws with.
//!
//! Colours live in [`Theme`](crate::Theme) by role. Everything else that has a
//! size lives here, so views never write a one-off `px(..)`:
//!
//! - [`space`]: gaps, padding and margins on a 4pt grid (plus 2 and 6).
//! - [`text`]: four type steps with paired line heights, and three weights.
//! - [`radius`]: concentric corners; each nested level is its parent minus the
//!   6px inset (20 → 14 → 8), a 10px field radius, and `rounded_full()` only
//!   for dots, avatars, status indicators and switches.
//! - [`size`]: control heights.
//! - [`icon`]: three icon sizes, all Phosphor regular.
//! - [`Elevation`]: three levels of lift; read the shadows with
//!   [`Theme::elevation`](crate::Theme::elevation).
//!
//! A value that is genuinely optical (the pane's close-dot centring maths, a
//! switch's knob) stays a named constant beside the component that needs it,
//! with a comment saying why.

use gpui::{FontWeight, Pixels, Styled, px};

/// Gaps, padding and margins.
pub mod space {
    use super::*;
    pub const X0_5: Pixels = px(2.);
    pub const X1: Pixels = px(4.);
    /// The panel → section → row inset used throughout both sidebars.
    pub const X1_5: Pixels = px(6.);
    pub const X2: Pixels = px(8.);
    pub const X3: Pixels = px(12.);
    pub const X4: Pixels = px(16.);
    pub const X6: Pixels = px(24.);
    pub const X8: Pixels = px(32.);
}

/// One step of the type ramp. `weight` is only set where the step implies one
/// (titles); otherwise the element inherits its weight.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypeStyle {
    pub size: Pixels,
    pub line: Pixels,
    pub weight: Option<FontWeight>,
}

/// Four type steps (ratio about 1.15). No half-pixel sizes.
pub mod text {
    use super::*;
    /// Times, paths, ids, tooltips, counts.
    pub const CAPTION: TypeStyle = TypeStyle {
        size: px(12.),
        line: px(16.),
        weight: None,
    };
    /// Secondary rows: mentions, search rows, reply chips, names.
    pub const SMALL: TypeStyle = TypeStyle {
        size: px(13.),
        line: px(18.),
        weight: None,
    };
    /// Messages, sidebar rows, pane body. The window's root size.
    pub const BODY: TypeStyle = TypeStyle {
        size: px(15.),
        line: px(22.),
        weight: None,
    };
    /// Channel header, pane title.
    pub const TITLE: TypeStyle = TypeStyle {
        size: px(17.),
        line: px(24.),
        weight: Some(FontWeight::SEMIBOLD),
    };

    /// Text.
    pub const REGULAR: FontWeight = FontWeight::NORMAL;
    /// Labels, names, buttons.
    pub const MEDIUM: FontWeight = FontWeight::MEDIUM;
    /// Titles, and the unread row.
    pub const SEMIBOLD: FontWeight = FontWeight::SEMIBOLD;
}

/// Corner radii, concentric with the 20px panels. Only dots, avatars, status
/// indicators and switches use `rounded_full()`; everything else is a rounded
/// square.
pub mod radius {
    use super::*;
    /// Tooltips.
    pub const SM: Pixels = px(6.);
    /// Buttons and rows: sidebar rows, agent tiles, message hover, search card,
    /// pane quick actions, the send button.
    pub const MD: Pixels = px(8.);
    /// Text fields: the titlebar search field, the composer.
    pub const FIELD: Pixels = px(10.);
    /// Sections inside a panel; the search panel (the field plus its 4px inset).
    pub const LG: Pixels = px(14.);
    /// Floating panels: sidebars, pane.
    pub const XL: Pixels = px(20.);
}

/// Control heights.
pub mod size {
    use super::*;
    /// Read-receipt avatars under a message.
    pub const READ_MARK: Pixels = px(16.);
    /// Chips, switches, the reply chip's dismiss button.
    pub const CHIP: Pixels = px(22.);
    /// Small buttons: titlebar toggles, "Open".
    pub const BUTTON_SM: Pixels = px(28.);
    /// Buttons: composer actions, avatars, the search pill.
    pub const BUTTON: Pixels = px(32.);
    /// List rows: channels, mentions, search rows, quick actions.
    pub const ROW: Pixels = px(36.);
    /// Primary rows: project picker, settings, agent tiles, quick links.
    pub const ROW_LG: Pixels = px(44.);
    /// Dividers.
    pub const HAIRLINE: Pixels = px(1.);
}

/// Icon sizes. One Phosphor weight (regular) everywhere.
pub mod icon {
    use super::*;
    /// Carets, dismiss crosses, inline marks.
    pub const SM: Pixels = px(12.);
    /// Default: titlebar, search, chat avatars, composer, channel rows.
    pub const MD: Pixels = px(16.);
    /// Primary sidebar rows, quick links, agent tiles.
    pub const LG: Pixels = px(20.);
}

/// How far a surface lifts off the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    /// Canvas, chat and rows inside a panel. Separation comes from a
    /// background step, never a border plus a shadow.
    None,
    /// Docked sidebars, the composer, the search panel.
    Float,
    /// Anything over the content: a peeking sidebar.
    Overlay,
}

/// Applies a [`TypeStyle`] to any element.
pub trait TypeStyleExt: Styled + Sized {
    fn type_style(self, style: TypeStyle) -> Self {
        let el = self.text_size(style.size).line_height(style.line);
        match style.weight {
            Some(weight) => el.font_weight(weight),
            None => el,
        }
    }
}

impl<T: Styled> TypeStyleExt for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_ramp_is_whole_pixels_and_ascending() {
        let ramp = [text::CAPTION, text::SMALL, text::BODY, text::TITLE];
        for pair in ramp.windows(2) {
            assert!(pair[0].size < pair[1].size);
            assert!(pair[0].line < pair[1].line);
        }
        for step in ramp {
            let size: f32 = step.size.into();
            let line: f32 = step.line.into();
            assert_eq!(size.fract(), 0., "no half-pixel font sizes");
            assert_eq!(line.fract(), 0.);
            assert!(line > size);
        }
    }

    #[test]
    fn radii_are_concentric_with_the_inset() {
        assert_eq!(radius::XL - space::X1_5, radius::LG);
        assert_eq!(radius::LG - space::X1_5, radius::MD);
        // The search panel wraps the field with a 4px inset.
        assert_eq!(radius::LG - space::X1, radius::FIELD);
        assert!(radius::SM < radius::MD && radius::MD < radius::FIELD);
    }
}
