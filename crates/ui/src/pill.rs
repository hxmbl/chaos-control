use gpui::{App, ElementId, InteractiveElement, Stateful, Styled, div};

use crate::ActiveTheme;

/// Thin-outlined pill shared by every row in both sidebars.
/// Callers add height, padding and content; hover is built in.
pub fn pill(id: impl Into<ElementId>, cx: &App) -> Stateful<gpui::Div> {
    let t = cx.theme();
    let (hover_bg, hover_line) = (t.hover, t.line_strong);
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .w_full()
        .rounded_full()
        .border_1()
        .border_color(t.line)
        .bg(t.surface)
        .text_color(t.ink)
        .hover(move |s| s.bg(hover_bg).border_color(hover_line))
}
