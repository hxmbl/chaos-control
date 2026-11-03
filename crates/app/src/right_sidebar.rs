//! Floating, detached activity panel mirroring the left sidebar: mentions and
//! agent status. Hidden below 1200px.
//!
//! Agent Status is a list of the project's members: each row is a model's
//! name with its harness's mark and its status dot (green active, blue
//! sleeping, red missing or unresponsive), and a remove button on hover for
//! roster members. The last row, "Add a model", opens the model picker.
//!
//! Permissions is the log of every permission request an agent made in
//! #main, newest first: the agent's mark (monochrome), the tool call's title,
//! how it ended ("Auto-allowed" or "Declined") and how long ago. Requests no
//! longer open the pane; clicking a row opens it read-only with the details.
//! The section hugs its rows, caps at five and scrolls past that, and is not
//! drawn while the log is empty.
//!
//! The wireframe's catch-up card ("Summarizing what happened while you were
//! gone…") is not drawn until summaries exist: a card that shimmers forever
//! for a feature that isn't there reads as fake, and it repainted every frame.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{Context, Entity, SharedString, Task, Window, div, prelude::*, px};
use model::{PaneContent, PermissionLogEntry, PermissionOutcome};
use ui::{
    ActiveTheme, Elevation, Ellipsis, IconName, Theme, Tooltip, TypeStyleExt, brand_icon, icon,
    pill,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::{model_picker::ModelPicker, pane_host::PaneHost, store::ChatStore};

/// How often the permission log's relative times ("4m") are redrawn.
const TICK: Duration = Duration::from_secs(30);

pub struct RightSidebar {
    store: Entity<ChatStore>,
    picker: Entity<ModelPicker>,
    pane: Entity<PaneHost>,
    mentions_open: bool,
    /// The Agent Status row under the pointer (shows its remove button).
    hovered_agent: Option<model::Id>,
    _tick: Task<()>,
}

impl RightSidebar {
    pub fn new(
        store: Entity<ChatStore>,
        picker: Entity<ModelPicker>,
        pane: Entity<PaneHost>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        // Relative times age without any event, so redraw now and then
        // (only while there is a log to show).
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let alive = this.update(cx, |this, cx| {
                    if !this.store.read(cx).permission_log().is_empty() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            store,
            picker,
            pane,
            mentions_open: true,
            hovered_agent: None,
            _tick: tick,
        }
    }

    /// One row of the permission log. Clicking it opens the pane read-only
    /// with the request's details.
    fn permission_row(
        &self,
        entry: PermissionLogEntry,
        now: u64,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let action: SharedString = entry
            .action
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
            .into();
        let when = model::relative_time(entry.at, now);
        let outcome = entry.outcome.label();
        let tip = format!(
            "{}: {} · {outcome} · #{}",
            entry.agent_name, entry.action, entry.request
        );
        let declined = entry.outcome == PermissionOutcome::Declined;
        let initial: SharedString = entry
            .agent_name
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_default()
            .into();
        let mark = div()
            .size(icons::LG)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .map(|d| match entry.brand {
                Some(brand) => d.child(icon(brand_icon(brand), icons::MD, t.ink_soft)),
                None => d
                    .type_style(text::CAPTION)
                    .font_weight(text::MEDIUM)
                    .text_color(t.ink_soft)
                    .child(initial),
            });
        pill(("permission", entry.request as usize), cx)
            .h(size::ROW)
            .flex_none()
            .gap(space::X2)
            .pl(space::X2)
            .pr(space::X2)
            .type_style(text::SMALL)
            .tooltip(Tooltip::text(tip))
            .on_click(cx.listener(move |this, _, window, cx| {
                // One thing in the pane layer at a time.
                this.picker.update(cx, |p, cx| {
                    if p.is_open() {
                        p.close(cx);
                    }
                });
                let content = PaneContent::Permission(entry.prompt());
                this.pane.update(cx, |p, cx| p.open(content, window, cx));
            }))
            .child(mark)
            .child(div().flex_1().min_w_0().ellipsis().child(truncated(action)))
            .child(
                div()
                    .flex_none()
                    .type_style(text::CAPTION)
                    .text_color(if declined { t.ink_soft } else { t.muted })
                    .child(outcome),
            )
            .child(
                div()
                    .flex_none()
                    .type_style(text::CAPTION)
                    .text_color(t.muted)
                    .child(when),
            )
    }
}

/// A permission row (`size::ROW`) plus the gap after it (`space::X0_5`).
const PERMISSION_ROW: f32 = 36. + 2.;
/// Rows shown before the permission log scrolls.
const PERMISSION_ROWS_SHOWN: f32 = 5.;

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
                let name = agent.map_or(model::PREVIOUS_AGENT.into(), |a| a.name.clone());
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
        let log: Vec<PermissionLogEntry> = self.store.read(cx).permission_log().entries().to_vec();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let permission_rows: Vec<_> = log
            .into_iter()
            .map(|entry| self.permission_row(entry, now, &t, cx))
            .collect();

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
            // Permissions: every request, newest first. Hugs its rows, caps
            // and scrolls like the left sidebar's channels; hidden when empty.
            .when(!permission_rows.is_empty(), |panel| {
                panel.child(
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
                                .child(truncated("Permissions")),
                        )
                        .child(
                            div()
                                .id("permission-rows")
                                .flex()
                                .flex_col()
                                .gap(space::X0_5)
                                .max_h(px(PERMISSION_ROW * PERMISSION_ROWS_SHOWN))
                                .overflow_y_scroll()
                                .children(permission_rows),
                        ),
                )
            })
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
