use gpui::{
    AnyView, App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, div, px,
};

use crate::{
    ActiveTheme, FONT_FAMILY, TypeStyleExt,
    tokens::{Elevation, radius, space, text},
};

/// Wrap long tooltips rather than letting them run across the window.
const MAX_W: gpui::Pixels = px(320.);

/// Live text for a tooltip that can change while it is visible.
///
/// GPUI builds a tooltip view when hover starts, so a tooltip made with
/// [`Tooltip::text`] keeps its original wording until the pointer leaves and
/// comes back. Hold this entity to update the text in place:
///
/// ```ignore
/// let tip = cx.new(|_| TooltipText::new("Collapse mentions"));
/// // in the button's click handler:
/// tip.update(cx, |tip, cx| tip.set("Expand mentions", cx));
/// // in the button's render:
/// button.tooltip(Tooltip::dynamic(tip.clone()))
/// ```
pub struct TooltipText {
    text: SharedString,
}

impl TooltipText {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self { text: text.into() }
    }

    /// Change the text and repaint any tooltip currently showing it.
    pub fn set(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.text = text.into();
        cx.notify();
    }
}

/// Small text tooltip, the GPUI stand-in for the web `title` attribute.
pub struct Tooltip {
    text: SharedString,
    /// When set, the live text wins over the snapshot taken at hover start.
    state: Option<Entity<TooltipText>>,
}

impl Tooltip {
    /// Builder for `.tooltip(..)` on any element with an id. The text is fixed
    /// when hover starts.
    pub fn text(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
        let text = text.into();
        move |_, cx| {
            cx.new(|_| Tooltip {
                text: text.clone(),
                state: None,
            })
            .into()
        }
    }

    /// Builder for a tooltip whose text can change while it is shown.
    /// Keeps showing the same view and follows updates to `state`.
    pub fn dynamic(state: Entity<TooltipText>) -> impl Fn(&mut Window, &mut App) -> AnyView {
        move |_, cx| {
            let state = state.clone();
            cx.new(|cx| {
                cx.observe(&state, |_, _, cx| cx.notify()).detach();
                Tooltip {
                    text: state.read(cx).text.clone(),
                    state: Some(state),
                }
            })
            .into()
        }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let text = self
            .state
            .as_ref()
            .map(|state| state.read(cx).text.clone())
            .unwrap_or_else(|| self.text.clone());
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
                .child(text),
        )
    }
}
