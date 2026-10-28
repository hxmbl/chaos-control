//! The pane's "liquid glass", faked in one place.
//!
//! gpui 0.2.2 can't blur what is behind an element (no backdrop filter), so the
//! glass is built from things it can draw:
//!
//! - a flat translucent fill (a gradient here was a 4-point alpha change
//!   nobody could see, so it went);
//! - an optional scrim over the content behind, which knocks that content back
//!   the way a blur would, so text under the pane never fights the pane's text;
//! - a hairline border plus a brighter 1px highlight along the top inner edge,
//!   the "lip" real glass has;
//! - a soft, wide shadow that lifts it off the chat.
//!
//! Everything that makes the pane look like glass comes from [`Glass`]. When a
//! GPUI with real backdrop blur lands, add the blur here, lower `fill`'s alpha
//! and drop the scrim; nothing that draws a pane needs to change.

use gpui::{BoxShadow, Hsla, point, px};

use crate::Theme;

/// Colours and shadow for one glass surface. `Copy`-cheap except the shadow.
#[derive(Clone, Debug)]
pub struct Glass {
    /// Flat translucent fill.
    pub fill: Hsla,
    /// Hairline border around the sheet.
    pub border: Hsla,
    /// 1px highlight along the top inner edge.
    pub highlight: Hsla,
    /// Wash over the content behind the pane (multiplied by the open progress).
    pub scrim: Hsla,
    /// Quick-action rows and other wells on the glass.
    pub well: Hsla,
    pub well_hover: Hsla,
    /// Divider above the quick actions.
    pub divider: Hsla,
    pub shadow: [BoxShadow; 2],
}

fn white(alpha: f32) -> Hsla {
    gpui::white().opacity(alpha)
}

fn black(alpha: f32) -> Hsla {
    gpui::black().opacity(alpha)
}

impl Theme {
    /// The pane's glass for this theme.
    pub fn glass(&self) -> Glass {
        if self.is_dark() {
            Glass {
                fill: self.selected.opacity(0.91),
                border: white(0.10),
                highlight: white(0.16),
                scrim: black(0.38),
                well: white(0.06),
                well_hover: white(0.11),
                divider: white(0.08),
                shadow: [
                    BoxShadow {
                        color: black(0.35),
                        offset: point(px(0.), px(2.)),
                        blur_radius: px(6.),
                        spread_radius: px(0.),
                    },
                    BoxShadow {
                        color: black(0.55),
                        offset: point(px(0.), px(28.)),
                        blur_radius: px(72.),
                        spread_radius: px(-8.),
                    },
                ],
            }
        } else {
            Glass {
                fill: self.surface.opacity(0.92),
                border: black(0.09),
                highlight: white(0.95),
                scrim: self.canvas.opacity(0.42),
                well: black(0.045),
                well_hover: black(0.08),
                divider: black(0.07),
                shadow: [
                    BoxShadow {
                        color: black(0.05),
                        offset: point(px(0.), px(1.)),
                        blur_radius: px(3.),
                        spread_radius: px(0.),
                    },
                    BoxShadow {
                        color: black(0.16),
                        offset: point(px(0.), px(24.)),
                        blur_radius: px(64.),
                        spread_radius: px(-8.),
                    },
                ],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glass_is_translucent_in_both_themes() {
        for theme in [Theme::light(), Theme::dark()] {
            let g = theme.glass();
            assert!(
                g.fill.a > 0.8 && g.fill.a < 1.,
                "fill lets the chat through, faintly"
            );
            assert!(
                g.scrim.a > 0. && g.scrim.a < 0.5,
                "scrim softens, never hides"
            );
        }
    }
}
