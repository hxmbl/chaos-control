//! Floating, detached activity panel mirroring the left sidebar: mentions and
//! agent status. Hidden below 1200px.
//!
//! Agent Status is a list of the project's members: each row is a model's
//! name with its harness's mark and its status dot (green active, blue
//! sleeping, red missing or unresponsive), and a remove button on hover for
//! roster members. The last row, "Add a model", opens the model picker.
//!
//! The wireframe's catch-up card ("Summarizing what happened while you were
//! gone…") is not drawn until summaries exist: a card that shimmers forever
//! for a feature that isn't there reads as fake, and it repainted every frame.

use gpui::{Context, Entity, SharedString, Window, div, prelude::*, px};
use ui::{
    ActiveTheme, Elevation, Ellipsis, IconName, Theme, Tooltip, TypeStyleExt, brand_icon, icon,
    pill,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::{model_picker::ModelPicker, store::ChatStore};

pub struct RightSidebar {
    store: Entity<ChatStore>,
    picker: Entity<ModelPicker>,
    mentions_open: bool,
    /// The Agent Status row under the pointer (shows its remove button).
    hovered_agent: Option<model::Id>,
}

impl RightSidebar {
    pub fn new(
        store: Entity<ChatStore>,
        picker: Entity<ModelPicker>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            picker,
            mentions_open: true,
            hovered_agent: None,
        }
    }
}

/// The status dot's diameter.
const DOT: f32 = 8.;

/// A mention row (`size::ROW`) plus the gap after it (`space::X1`).
const MENTION_ROW: f32 = 36. + 4.;
/// The mentions section's toggle row plus its padding.
const MENTIONS_CHROME: f32 = 50.;

/// A section is a background step only: no outline inside the panel's.
fn section(t: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .rounded(radius::LG)
        .bg(t.sunken)
}

impl Render for RightSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let ws = self.store.read(cx).workspace();

        let mention_rows: Vec<_> = ws
            .mentions
            .iter()
            .map(|m| {
                let agent = ws.agent(m.agent);
                let name = agent.map_or("Agent".into(), |a| a.name.clone());
                (m.channel, agent.map(|a| a.brand), name, m.text.clone())
            })
            .collect();
        let agents: Vec<_> = ws
            .agents
            .iter()
            .map(|a| {
                (
                    a.id,
                    a.name.clone(),
                    a.brand,
                    a.state,
                    a.status_line(),
                    a.removable,
                )
            })
            .collect();
        let can_add = self.store.read(cx).has_core();
        let count = mention_rows.len();
        let open = self.mentions_open;

        div()
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(space::X1_5)
            .p(space::X1_5)
            .rounded(radius::XL)
            .border_1()
            .border_color(t.line)
            .bg(t.surface)
            .shadow(t.elevation(Elevation::Float))
            // Mentions: grows to show more rows, but never past its content.
            .child(
                section(&t)
                    .gap(space::X1)
                    .p(space::X1_5)
                    .when(open, |s| {
                        s.flex_grow()
                            .flex_shrink()
                            .flex_basis(px(MENTION_ROW * 3. + MENTIONS_CHROME))
                            .min_h(px(MENTION_ROW + MENTIONS_CHROME))
                            .max_h(px(MENTION_ROW * count as f32 + MENTIONS_CHROME))
                    })
                    .child(
                        pill("mentions-toggle", cx)
                            .h(size::ROW)
                            .justify_center()
                            .gap(space::X2)
                            .text_color(t.ink_soft)
                            .tooltip(Tooltip::text(if open {
                                "Collapse mentions"
                            } else {
                                "Expand mentions"
                            }))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mentions_open = !this.mentions_open;
                                cx.notify();
                            }))
                            .when(!open, |p| {
                                p.child(
                                    div()
                                        .type_style(text::SMALL)
                                        .font_weight(text::MEDIUM)
                                        .child(format!(
                                            "{count} {}",
                                            if count == 1 { "mention" } else { "mentions" }
                                        )),
                                )
                            })
                            .child(icon(
                                if open {
                                    IconName::CaretUp
                                } else {
                                    IconName::CaretDown
                                },
                                icons::SM,
                                t.ink_soft,
                            )),
                    )
                    .when(open, |s| {
                        s.child(
                            div()
                                .id("mentions")
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap(space::X1)
                                .children(mention_rows.into_iter().enumerate().map(
                                    |(ix, (channel, brand, name, text))| {
                                        let tip = format!("{name}: {text}");
                                        pill(("mention", ix), cx)
                                            .h(size::ROW)
                                            .gap(space::X2)
                                            .pl(space::X2)
                                            .pr(space::X3)
                                            .type_style(text::SMALL)
                                            .tooltip(Tooltip::text(tip))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.store
                                                    .update(cx, |s, cx| s.select(channel, cx));
                                            }))
                                            .when_some(brand, |p, brand| {
                                                p.child(
                                                    div()
                                                        .size(icons::LG)
                                                        .flex_none()
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .child(icon(
                                                            brand_icon(brand),
                                                            icons::MD,
                                                            t.ink,
                                                        )),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .ellipsis()
                                                    .child(truncated(SharedString::from(text))),
                                            )
                                    },
                                )),
                        )
                    }),
            )
            // Spare height sits here, so agent status stays at the bottom
            // like the left sidebar's settings row.
            .child(div().flex_1())
            // Agent status. Marks are monochrome: colour here only means
            // state (the dot).
            .child(
                section(&t)
                    .gap(space::X1)
                    .px(space::X1_5)
                    .pt(space::X3)
                    .pb(space::X1_5)
                    .child(
                        div()
                            .px(space::X2)
                            .pb(space::X1)
                            .ellipsis()
                            .type_style(text::SMALL)
                            .font_weight(text::MEDIUM)
                            .text_color(t.ink_soft)
                            .child(truncated("Agent status")),
                    )
                    .child(
                        div()
                            .id("agent-rows")
                            .flex()
                            .flex_col()
                            .gap(space::X0_5)
                            .max_h(px(MENTION_ROW * 6.))
                            .overflow_y_scroll()
                            .children(agents.into_iter().map(
                                |(id, name, brand, state, label, removable)| {
                                    let hovered = self.hovered_agent == Some(id);
                                    let dot = t.agent_state(state);
                                    pill(("agent", id.0 as usize), cx)
                                        .h(size::ROW)
                                        .gap(space::X2)
                                        .pl(space::X2)
                                        .pr(space::X2)
                                        .tooltip(Tooltip::text(label))
                                        .on_hover(cx.listener(move |this, over: &bool, _, cx| {
                                            let now = if *over { Some(id) } else { None };
                                            if *over || this.hovered_agent == Some(id) {
                                                this.hovered_agent = now;
                                                cx.notify();
                                            }
                                        }))
                                        .child(icon(brand_icon(brand), icons::MD, t.ink_soft))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .ellipsis()
                                                .type_style(text::SMALL)
                                                .child(truncated(SharedString::from(name))),
                                        )
                                        .when(hovered && removable, |row| {
                                            row.child(
                                                div()
                                                    .id(("agent-remove", id.0 as usize))
                                                    .flex()
                                                    .flex_none()
                                                    .items_center()
                                                    .justify_center()
                                                    .size(size::CHIP)
                                                    .rounded(radius::SM)
                                                    .hover(|s| s.bg(t.selected))
                                                    .tooltip(Tooltip::text("Remove from this project"))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        cx.stop_propagation();
                                                        this.hovered_agent = None;
                                                        this.store.update(cx, |s, cx| {
                                                            s.remove_agent(id, cx)
                                                        });
                                                    }))
                                                    .child(icon(IconName::X, icons::SM, t.ink_soft)),
                                            )
                                        })
                                        .child(
                                            div()
                                                .flex_none()
                                                .size(px(DOT))
                                                .rounded_full()
                                                .bg(dot)
                                                .shadow(t.glow_shadow(dot)),
                                        )
                                },
                            )),
                    )
                    .when(can_add, |s| {
                        s.child(
                            pill("agent-add", cx)
                                .h(size::ROW)
                                .gap(space::X2)
                                .px(space::X2)
                                .text_color(t.ink_soft)
                                .type_style(text::SMALL)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.picker.update(cx, |p, cx| p.open(window, cx));
                                }))
                                .child(icon(IconName::Plus, icons::MD, t.ink_soft))
                                .child("Add a model"),
                        )
                    }),
            )
    }
}
