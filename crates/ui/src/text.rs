//! Single-line text that ends in "…" instead of being clipped mid-glyph.

use gpui::Styled;

/// `.ellipsis()` on any styled element: one line, cut with "…" when it would
/// overflow. Use it on the flex child that holds the text (it adds `min_w_0`, so
/// the child can shrink below its content width).
///
/// Why not GPUI's own `truncate()`: in gpui 0.2.2 a non-wrapping text element
/// keeps the first size it measures, and in a flex row that first pass is often
/// the unconstrained one. The ellipsis is then never applied and
/// `overflow_hidden` clips the glyphs instead. Wrapping text is re-measured
/// whenever its width changes, so this clamps a wrapping line to one line and
/// lets the ellipsis come from that path.
pub trait Ellipsis: Styled + Sized {
    fn ellipsis(self) -> Self {
        self.min_w_0()
            .overflow_hidden()
            .whitespace_normal()
            .line_clamp(1)
            .text_ellipsis()
    }
}

impl<T: Styled> Ellipsis for T {}
