use gpui::{
    AnyView, App, Context, IntoElement, ParentElement, Render, SharedString, Styled, Window, div,
    px,
};

use crate::{ActiveTheme, FONT_FAMILY};

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
        div().pl(px(8.)).pt(px(6.)).child(
            div()
                .font_family(FONT_FAMILY)
                .text_size(px(12.))
                .line_height(px(16.))
                .px(px(8.))
                .py(px(4.))
                .max_w(px(320.))
                .rounded(px(8.))
                .bg(t.ink)
                .text_color(t.surface)
                .shadow(t.float_shadow())
                .child(self.text.clone()),
        )
    }
}
