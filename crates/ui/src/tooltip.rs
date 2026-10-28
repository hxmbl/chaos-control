use gpui::{
    AnyView, App, AppContext, Context, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, div, px,
};

use crate::{
    ActiveTheme, FONT_FAMILY, TypeStyleExt,
    tokens::{radius, space, text},
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
        // Inverted chip so it reads on either sidebar surface.
        // No shadow: the inverted chip already separates itself.
        div().pl(space::X2).pt(space::X1_5).child(
            div()
                .font_family(FONT_FAMILY)
                .type_style(text::CAPTION)
                .px(space::X2)
                .py(space::X1)
                .max_w(MAX_W)
                .rounded(radius::SM)
                .bg(t.ink)
                .text_color(t.surface)
                .child(self.text.clone()),
        )
    }
}
