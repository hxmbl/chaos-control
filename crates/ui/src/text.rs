//! Single-line text that ends in "…" instead of being clipped mid-glyph.

use gpui::{
    App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, ShapedLine, SharedString, Size, Style, Styled, Window, px, size,
};

/// Text that stays on one line and ends in "…" when its box is too narrow.
/// Font, size, weight and colour come from the surrounding text style.
///
/// Put it in a flex child marked `.ellipsis()` so the child can shrink:
/// `div().flex_1().ellipsis().child(truncated(name))`.
///
/// Why not GPUI's `truncate()`: in gpui 0.2.2 the text element truncates while
/// it is being measured, keeps the first size it measures, and shortens its
/// style runs in place. In a flex row the first pass is often unconstrained (no
/// ellipsis, glyphs clipped) or very narrow (later text loses its weight or
/// colour). This element measures the full line and cuts it once, in prepaint,
/// against the width it was actually given.
pub fn truncated(text: impl Into<SharedString>) -> Truncated {
    Truncated { text: text.into() }
}

pub struct Truncated {
    text: SharedString,
}

/// `.ellipsis()` on the flex child that holds a [`truncated`] label: lets it
/// shrink below its content width and clips anything that still overflows.
pub trait Ellipsis: Styled + Sized {
    fn ellipsis(self) -> Self {
        self.min_w_0().overflow_hidden().whitespace_nowrap()
    }
}

impl<T: Styled> Ellipsis for T {}

impl IntoElement for Truncated {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Truncated {
    type RequestLayoutState = ();
    type PrepaintState = Option<ShapedLine>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let runs = vec![style.to_run(self.text.len())];
        let full_width = window
            .text_system()
            .shape_line(self.text.clone(), font_size, &runs, None)
            .width
            .ceil();
        // Intrinsic width is the whole line; given a narrower box, take the box.
        let layout_id = window.request_measured_layout(
            Style::default(),
            move |known: Size<Option<Pixels>>, available: Size<AvailableSpace>, _, _| {
                let width = known.width.unwrap_or(match available.width {
                    AvailableSpace::Definite(w) => full_width.min(w),
                    _ => full_width,
                });
                size(width, known.height.unwrap_or(line_height))
            },
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let mut runs = vec![style.to_run(self.text.len())];
        let line = window
            .text_system()
            .shape_line(self.text.clone(), font_size, &runs, None);
        if line.width <= bounds.size.width + px(0.5) {
            return Some(line);
        }
        let cut = cx
            .text_system()
            .line_wrapper(style.font(), font_size)
            .truncate_line(self.text.clone(), bounds.size.width, "…", &mut runs);
        Some(window.text_system().shape_line(cut, font_size, &runs, None))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        line: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(line) = line.take() {
            line.paint(bounds.origin, window.line_height(), window, cx)
                .ok();
        }
    }
}
