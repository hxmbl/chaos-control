//! Floating, detached activity panel mirroring the left sidebar: mentions and
//! agent status. Hidden below 1200px.
//!
//! The Agents section lists the project's members: each row is a model's
//! name with its harness's mark and its status dot (green active, blue
//! sleeping, red missing or unresponsive), and a remove button on hover for
//! roster members. The last row, "Add a model", opens the model picker.
//!
//! The wireframe's catch-up card ("Summarizing what happened while you were
//! gone…") is not drawn until summaries exist: a card that shimmers forever
//! for a feature that isn't there reads as fake, and it repainted every frame.

use gpui::{Context, Entity, SharedString, Window, div, prelude::*, px};
use ui::{
    ActiveTheme, Elevation, Ellipsis, IconName, Theme, Tooltip, TooltipText, TypeStyleExt,
    brand_icon, icon, pill,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::{model_picker::ModelPicker, store::ChatStore};

pub struct RightSidebar {
    store: Entity<ChatStore>,
    picker: Entity<ModelPicker>,
    mentions_open: bool,
    /// The toggle tooltip follows the click, even while the pointer is still
    /// over the button and the tooltip is on screen.
    mentions_tip: Entity<TooltipText>,
    /// The Agents row under the pointer (shows its remove button).
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
            mentions_tip: cx.new(|_| TooltipText::new("Collapse mentions")),
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

/// "21:01" for a receipt time, seconds since the epoch.
fn clock(secs: u64) -> String {
    let minutes = secs / 60;
    format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60)
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
                let name = agent.map_or(model::PREVIOUS_AGENT.into(), |a| a.name.clone());
                (m.channel, agent.map(|a| a.brand), name, m.text.clone())
            })
            .collect();
        // Read state is for the channel on screen: a check when the agent has
        // seen the newest message, a count when it is behind.
        let channel = self.store.read(cx).selected();
        let channel_name = ws.channel(channel).map(|c| c.name.clone());
        let agents: Vec<_> = ws
            .agents
            .iter()
            .map(|a| {
                let seen = self.store.read(cx).agent_seen(channel, a.id);
                let mut label = a.status_line();
                let nicks = self.store.read(cx).agent_nicknames(a.id);
                if !nicks.is_empty() {
                    let list: Vec<String> = nicks.iter().map(|n| format!("@{n}")).collect();
                    label.push_str(&format!(" · also {}", list.join(" ")));
                }
                if let Some(seen) = seen {
                    let chan = channel_name.as_deref().unwrap_or("this channel");
                    if seen.behind == 0 {
                        label.push_str(&format!(" · seen {chan} at {}", clock(seen.read_at)));
                    } else {
                        label.push_str(&format!(" · {} unread in {chan}", seen.behind));
                    }
                }
                (
                    a.id,
                    a.name.clone(),
                    a.brand,
                    a.state,
                    label,
                    a.removable,
                    seen,
                )
            })
            .collect();
        let can_add = self.store.read(cx).has_core();
        // Blocked approvals (STRM-612): show the outcome instead of dropping it.
        let notices: Vec<_> = self
            .store
            .read(cx)
            .approval_notices()
            .iter()
            .map(|n| {
                (
                    n.label(),
                    n.error
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                )
            })
            .collect();
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
                            .tooltip(Tooltip::dynamic(self.mentions_tip.clone()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                let open = !this.mentions_open;
                                this.mentions_open = open;
                                let label = if open {
                                    "Collapse mentions"
                                } else {
                                    "Expand mentions"
                                };
                                this.mentions_tip.update(cx, |tip, cx| tip.set(label, cx));
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
            // Blocked approvals: fail loudly in the sidebar (STRM-612). The
            // core outcome used to be dropped after clearing the pending pane.
            .when(!notices.is_empty(), |root| {
                root.child(
                    section(&t)
                        .gap(space::X1)
                        .px(space::X1_5)
                        .py(space::X2)
                        .child(
                            div()
                                .px(space::X2)
                                .type_style(text::SMALL)
                                .font_weight(text::MEDIUM)
                                .text_color(t.ink_soft)
                                .child(truncated("Blocked approvals")),
                        )
                        .child(
                            div()
                                .id("approval-notices")
                                .flex()
                                .flex_col()
                                .gap(space::X0_5)
                                .max_h(px(MENTION_ROW * 3.))
                                .overflow_y_scroll()
                                .children(notices.into_iter().enumerate().map(
                                    |(i, (label, error))| {
                                        let tip = if error.is_empty() {
                                            label.to_string()
                                        } else {
                                            format!("{label}: {error}")
                                        };
                                        pill(("approval-notice", i), cx)
                                            .h(size::ROW)
                                            .gap(space::X2)
                                            .px(space::X2)
                                            .tooltip(Tooltip::text(tip))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .ellipsis()
                                                    .type_style(text::CAPTION)
                                                    .text_color(t.ink)
                                                    .child(truncated(SharedString::from(label))),
                                            )
                                            .when(!error.is_empty(), |row| {
                                                row.child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .ellipsis()
                                                        .type_style(text::CAPTION)
                                                        .text_color(t.ink_soft)
                                                        .child(truncated(SharedString::from(
                                                            error,
                                                        ))),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .size(px(DOT))
                                                    .rounded_full()
                                                    .bg(t.status_unresponsive)
                                                    .shadow(t.glow_shadow(t.status_unresponsive)),
                                            )
                                    },
                                )),
                        ),
                )
            })
            // Spare height sits here, so agent status stays at the bottom
            // like the left sidebar's settings row.
            .child(div().flex_1())
            // Agents. Marks are monochrome: colour here only means
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
                            .child(truncated("Agents")),
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
                                |(id, name, brand, state, label, removable, seen)| {
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
                                        // Seen everything in the open
                                        // channel: a quiet check. Behind: how
                                        // many messages it has not read yet.
                                        .when_some(seen, |row, seen| {
                                            row.child(if seen.behind == 0 {
                                                icon(IconName::Check, icons::SM, t.muted)
                                                    .into_any_element()
                                            } else {
                                                div()
                                                    .flex_none()
                                                    .type_style(text::CAPTION)
                                                    .font_weight(text::MEDIUM)
                                                    .text_color(t.ink_soft)
                                                    .child(if seen.behind > 99 {
                                                        SharedString::from("99+")
                                                    } else {
                                                        SharedString::from(seen.behind.to_string())
                                                    })
                                                    .into_any_element()
                                            })
                                        })
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
                                                    .tooltip(Tooltip::text(
                                                        "Remove from this project",
                                                    ))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        cx.stop_propagation();
                                                        this.hovered_agent = None;
                                                        this.store.update(cx, |s, cx| {
                                                            s.remove_agent(id, cx)
                                                        });
                                                    }))
                                                    .child(icon(
                                                        IconName::X,
                                                        icons::SM,
                                                        t.ink_soft,
                                                    )),
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
                                // A second click closes the picker rather
                                // than wiping what was typed in it.
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.picker.update(cx, |p, cx| {
                                        if p.is_open() {
                                            p.close(cx)
                                        } else {
                                            p.open(window, cx)
                                        }
                                    });
                                }))
                                .child(icon(IconName::Plus, icons::MD, t.ink_soft))
                                .child("Add a model"),
                        )
                    }),
            )
    }
}
