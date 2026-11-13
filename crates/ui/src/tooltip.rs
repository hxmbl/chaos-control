use gpui::{
    AnyView, App, AppContext, Context, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, div, px,
};

use crate::{
    ActiveTheme, FONT_FAMILY, TypeStyleExt,
    tokens::{Elevation, radius, space, text},
};

/// Wrap long tooltips rather than letting them run across the window.
const MAX_W: gpui::Pixels = px(320.);

/// Small text tooltip, the GPUI stand-in for the web `title` attribute.
pub struct Tooltip {
    text: SharedString,
}

impl Tooltip {
    /// Builder for `.tooltip(..)` on any element with an id.
    pub fn text(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
        let text = text.into();
        move |_, cx| cx.new(|_| Tooltip { text: text.clone() }).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        // Same recipe as the composer's mention menu: a raised surface with a
        // border and an overlay shadow, so it follows the theme. The border is
        // `line_strong` because tooltips often sit over a `surface` panel. The
        // old inverted chip (ink fill, surface text) became a bright white
        // block in dark mode.
        div().pl(space::X2).pt(space::X1_5).child(
            div()
                .font_family(FONT_FAMILY)
                .type_style(text::CAPTION)
                .px(space::X2)
                .py(space::X1)
                .max_w(MAX_W)
                .rounded(radius::SM)
                .bg(t.surface)
                .border_1()
                .border_color(t.line_strong)
                .shadow(t.elevation(Elevation::Overlay))
                .text_color(t.ink)
                .child(self.text.clone()),
        )
    }
}
