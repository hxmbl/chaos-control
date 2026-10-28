use gpui::{App, ElementId, InteractiveElement, Stateful, Styled, div};

use crate::ActiveTheme;

/// Borderless pill shared by the rows in both sidebars. It has no outline and
/// no fill of its own: it sits on whatever it is placed on, shows `hover` under
/// the pointer, and callers mark the selected row with `bg(t.selected)`.
/// Callers add height, padding and content.
pub fn pill(id: impl Into<ElementId>, cx: &App) -> Stateful<gpui::Div> {
    let t = cx.theme();
    let hover_bg = t.hover;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .w_full()
        .rounded_full()
        .text_color(t.ink)
        .hover(move |s| s.bg(hover_bg))
}
