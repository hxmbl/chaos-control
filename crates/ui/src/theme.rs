//! Calm palette from the wireframe: generous radii on a warm off-white, with
//! separation from background steps rather than outlines. Colour carries
//! meaning only: `accent` is "you / unread / on", `status_*` is agent state.
//! Dark mode follows the OS appearance. Sizes live in [`crate::tokens`].

use gpui::{App, BoxShadow, Global, Hsla, WindowAppearance, point, px, rgb};
use model::AgentState;

use crate::tokens::Elevation;

/// Bundled UI font (see `assets/fonts`).
pub const FONT_FAMILY: &str = "Plus Jakarta Sans";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

impl From<WindowAppearance> for Appearance {
    fn from(value: WindowAppearance) -> Self {
        match value {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
        }
    }
}

/// Every colour the shell uses. Copy-only fields so reading the theme never allocates.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub appearance: Appearance,
    pub canvas: Hsla,
    pub surface: Hsla,
    pub sunken: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub line: Hsla,
    pub line_strong: Hsla,
    pub ink: Hsla,
    pub ink_soft: Hsla,
    pub muted: Hsla,
    pub faint: Hsla,
    /// "You / unread / on": the unread dot, reply bar, switches, selection.
    /// Never used for agent state.
    pub accent: Hsla,
    /// Text and glyphs drawn on `accent` (the switch knob).
    pub on_accent: Hsla,
    /// Agent state, one per `AgentState`: green active, blue sleeping, red
    /// unresponsive. The only place colour means status.
    pub status_active: Hsla,
    pub status_sleeping: Hsla,
    pub status_unresponsive: Hsla,
    pub selection: Hsla,
    shadow_near: Hsla,
    shadow_far: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

impl Theme {
    pub fn light() -> Self {
        Self {
            appearance: Appearance::Light,
            canvas: c(0xf6f5f2),
            surface: c(0xffffff),
            sunken: c(0xf1f0ec),
            hover: c(0xf3f2ee),
            selected: c(0xecebe6),
            line: c(0xe2e0da),
            line_strong: c(0xcfccc4),
            ink: c(0x1f1e1c),
            ink_soft: c(0x4a4843),
            muted: c(0x8a867e),
            faint: c(0xb3afa6),
            accent: c(0x6e56cf),
            on_accent: c(0xffffff),
            status_active: c(0x22b35e),
            status_sleeping: c(0x3b82f6),
            status_unresponsive: c(0xef4444),
            selection: c(0x6e56cf).opacity(0.22),
            shadow_near: c(0x1f1e1c).opacity(0.04),
            shadow_far: c(0x1f1e1c).opacity(0.12),
        }
    }

    pub fn dark() -> Self {
        Self {
            appearance: Appearance::Dark,
            canvas: c(0x151514),
            surface: c(0x1c1c1a),
            sunken: c(0x191917),
            hover: c(0x242422),
            selected: c(0x2b2b28),
            line: c(0x2f2e2b),
            line_strong: c(0x43423e),
            ink: c(0xedece8),
            ink_soft: c(0xc9c6bf),
            muted: c(0x8f8b83),
            faint: c(0x5f5c56),
            accent: c(0x9b87f5),
            on_accent: c(0xffffff),
            status_active: c(0x3ddc84),
            status_sleeping: c(0x60a5fa),
            status_unresponsive: c(0xff5c5c),
            selection: c(0x9b87f5).opacity(0.3),
            shadow_near: c(0x000000).opacity(0.3),
            shadow_far: c(0x000000).opacity(0.5),
        }
    }

    pub fn for_appearance(appearance: impl Into<Appearance>) -> Self {
        match appearance.into() {
            Appearance::Light => Self::light(),
            Appearance::Dark => Self::dark(),
        }
    }

    pub fn is_dark(&self) -> bool {
        self.appearance == Appearance::Dark
    }

    /// Soft drop shadow for the floating panels and the composer.
    pub fn float_shadow(&self) -> Vec<BoxShadow> {
        let (y, blur) = if self.is_dark() {
            (12., 32.)
        } else {
            (8., 24.)
        };
        vec![
            BoxShadow {
                color: self.shadow_near,
                offset: point(px(0.), px(1.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
            },
            BoxShadow {
                color: self.shadow_far,
                offset: point(px(0.), px(y)),
                blur_radius: px(blur),
                spread_radius: px(-12.),
            },
        ]
    }

    /// Deeper shadow for a sidebar peeking over the content, so it reads as
    /// floating above the chat rather than docked beside it.
    pub fn overlay_shadow(&self) -> Vec<BoxShadow> {
        let (y, blur) = if self.is_dark() {
            (16., 44.)
        } else {
            (14., 40.)
        };
        vec![
            BoxShadow {
                color: self.shadow_near,
                offset: point(px(0.), px(2.)),
                blur_radius: px(6.),
                spread_radius: px(0.),
            },
            BoxShadow {
                color: self.shadow_far,
                offset: point(px(0.), px(y)),
                blur_radius: px(blur),
                spread_radius: px(-6.),
            },
        ]
    }

    /// Shadows for an [`Elevation`] level.
    pub fn elevation(&self, level: Elevation) -> Vec<BoxShadow> {
        match level {
            Elevation::None => Vec::new(),
            Elevation::Float => self.float_shadow(),
            Elevation::Overlay => self.overlay_shadow(),
        }
    }

    /// Status colour for an agent's state: green active, blue sleeping, red
    /// unresponsive.
    pub fn agent_state(&self, state: AgentState) -> Hsla {
        match state {
            AgentState::Active => self.status_active,
            AgentState::Sleeping => self.status_sleeping,
            AgentState::Unresponsive => self.status_unresponsive,
        }
    }

    /// Status glow pooled under an agent tile: one soft layer, no hard
    /// coloured lip, so eight of them read as a hint of colour rather than a
    /// grid of halos. On a dark surface the glow reads as light, so it spreads
    /// a little more.
    pub fn glow_shadow(&self, glow: Hsla) -> Vec<BoxShadow> {
        let (y, blur, spread, alpha) = if self.is_dark() {
            (4., 12., -5., 0.32)
        } else {
            (4., 10., -5., 0.34)
        };
        vec![BoxShadow {
            color: glow.opacity(alpha),
            offset: point(px(0.), px(y)),
            blur_radius: px(blur),
            spread_radius: px(spread),
        }]
    }
}

impl Global for Theme {}

/// `cx.theme()` from any context that derefs to `App`.
pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

pub(crate) fn init(cx: &mut App) {
    let theme = Theme::for_appearance(cx.window_appearance());
    cx.set_global(theme);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_colours_never_reuse_the_accent() {
        for t in [Theme::light(), Theme::dark()] {
            for state in [
                AgentState::Active,
                AgentState::Sleeping,
                AgentState::Unresponsive,
            ] {
                assert_ne!(t.agent_state(state), t.accent, "{state:?}");
            }
            // Sleeping is blue, well away from the purple accent.
            let hue = |c: Hsla| c.h * 360.;
            assert!((200. ..=230.).contains(&hue(t.status_sleeping)));
            assert!((hue(t.status_sleeping) - hue(t.accent)).abs() > 25.);
        }
    }

    #[test]
    fn status_glow_is_one_soft_layer() {
        for t in [Theme::light(), Theme::dark()] {
            let glow = t.glow_shadow(t.status_active);
            assert_eq!(glow.len(), 1);
            assert!(glow[0].blur_radius > px(0.), "no hard lip");
        }
    }
}
