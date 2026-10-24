//! Calm, outline-first palette from the wireframe: thin lines, pills and generous
//! radii on a warm off-white. Dark mode follows the OS appearance.

use gpui::{App, BoxShadow, Global, Hsla, WindowAppearance, point, px, rgb};

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
    pub accent: Hsla,
    pub status_working: Hsla,
    pub status_idle: Hsla,
    pub status_blocked: Hsla,
    pub skeleton: Hsla,
    pub skeleton_shine: Hsla,
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
            status_working: c(0x22b35e),
            status_idle: c(0x4f6bff),
            status_blocked: c(0xef4444),
            skeleton: c(0xd2cfc7),
            skeleton_shine: c(0xe9e7e1),
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
            status_working: c(0x3ddc84),
            status_idle: c(0x7b8cff),
            status_blocked: c(0xff5c5c),
            skeleton: c(0x393834),
            skeleton_shine: c(0x4d4b46),
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

    /// Status glow pooled under an agent pill. On a dark surface the glow reads as
    /// light, so it spreads a little more.
    pub fn glow_shadow(&self, glow: Hsla) -> Vec<BoxShadow> {
        let (y, blur, spread, a1, a2) = if self.is_dark() {
            (6., 16., -6., 0.70, 0.55)
        } else {
            (5., 12., -5., 0.75, 0.45)
        };
        vec![
            BoxShadow {
                color: glow.opacity(a1),
                offset: point(px(0.), px(y)),
                blur_radius: px(blur),
                spread_radius: px(spread),
            },
            BoxShadow {
                color: glow.opacity(a2),
                offset: point(px(0.), px(2.)),
                blur_radius: px(0.),
                spread_radius: px(-1.),
            },
        ]
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
