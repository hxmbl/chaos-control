//! Floating, detached left sidebar: project picker, channels, quick links, settings.

use gpui::{Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px, uniform_list};
use model::{Channel, PaneContent};
use ui::{ActiveTheme, Ellipsis, IconName, Tooltip, brand_icon, icon, pill, truncated};

use crate::{pane_host::PaneHost, store::ChatStore};

/// Channel pill height plus the gap below it; `uniform_list` needs one fixed row height.
const CHANNEL_ROW: f32 = 36. + 6.;

pub struct Sidebar {
    store: Entity<ChatStore>,
    pane: Entity<PaneHost>,
    /// The project picker's list is showing.
    picker_open: bool,
}

impl Sidebar {
    pub fn new(store: Entity<ChatStore>, pane: Entity<PaneHost>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            pane,
            picker_open: false,
        }
    }

    /// The picker's list: every project (from core when it runs, else the
    /// sample), the open one highlighted. A window shows the project of the
    /// folder it was launched from, so the others are listed with their
    /// folders rather than switched to here.
    fn render_projects(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = *cx.theme();
        let entries = self.store.read(cx).workspace().projects.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(6.))
            .rounded(px(21.))
            .border_1()
            .border_color(t.line)
            .bg(t.sunken)
            .children(entries.into_iter().enumerate().map(|(ix, entry)| {
                let path = entry.path.clone();
                pill(("project-entry", ix), cx)
                    .h(px(44.))
                    .px(px(12.))
                    .flex_col()
                    .items_start()
                    .justify_center()
                    .when(entry.current, |p| {
                        p.border_color(t.line_strong)
                            .bg(t.selected)
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!entry.current, |p| {
                        p.tooltip(Tooltip::text(match &path {
                            Some(path) => format!("Launch Chaos Control in {path} to open it"),
                            None => "Not linked to a folder on this computer".to_string(),
                        }))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picker_open = false;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w_full()
                            .ellipsis()
                            .text_size(px(14.))
                            .line_height(px(18.))
                            .child(truncated(SharedString::from(entry.name.clone()))),
                    )
                    .child(
                        div()
                            .w_full()
                            .ellipsis()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(t.muted)
                            .child(truncated(SharedString::from(
                                path.unwrap_or_else(|| "No folder".into()),
                            ))),
                    )
            }))
    }

    fn render_channel(
        &self,
        channel: &Channel,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let t = *cx.theme();
        let id = channel.id;
        let name = SharedString::from(channel.name.clone());
        let bold = selected || channel.unread;

        div().h(px(CHANNEL_ROW)).pb(px(6.)).child(
            pill(("channel", id.0 as usize), cx)
                .h(px(36.))
                .gap(px(8.))
                .pl(px(12.))
                .pr(px(8.))
                .when(selected, |p| p.border_color(t.line_strong).bg(t.selected))
                .when(bold, |p| p.font_weight(FontWeight::SEMIBOLD))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.store.update(cx, |store, cx| store.select(id, cx));
                }))
                .child(if selected {
                    icon(IconName::HashBold, px(17.), t.ink).into_any_element()
                } else {
                    icon(IconName::Hash, px(17.), t.ink_soft).into_any_element()
                })
                .child(div().flex_1().ellipsis().child(truncated(name)))
                .when(channel.unread, |p| {
                    p.child(div().size(px(6.)).flex_none().rounded_full().bg(t.accent))
                })
                .when_some(channel.linear_issue.clone(), |p, issue| {
                    p.child(
                        div()
                            .id(("linear", id.0 as usize))
                            .size(px(20.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .tooltip(Tooltip::text(format!("Linked to Linear issue {issue}")))
                            .child(icon(IconName::BrandLinear, px(15.), t.muted)),
                    )
                }),
        )
    }
}

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let store = self.store.read(cx);
        let ws = store.workspace();
        let project_emoji = SharedString::from(ws.project.emoji.clone());
        let project_name = SharedString::from(ws.project.name.clone());
        let channel_count = ws.channels.len();
        let quick_links: Vec<_> = ws
            .quick_links
            .iter()
            .map(|l| (l.label.clone(), l.brand, l.url.clone()))
            .collect();

        let section = || {
            div()
                .rounded(px(21.))
                .border_1()
                .border_color(t.line)
                .bg(t.sunken)
                .p(px(6.))
        };

        div()
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(6.))
            .rounded(px(26.))
            .border_1()
            .border_color(t.line)
            .bg(t.surface)
            .shadow(t.float_shadow())
            // Project picker
            .child(
                pill("project", cx)
                    .h(px(44.))
                    .gap(px(8.))
                    .px(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .tooltip(Tooltip::text("Projects"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picker_open = !this.picker_open;
                        cx.notify();
                    }))
                    .child(div().text_size(px(16.)).child(project_emoji))
                    .child(div().flex_1().ellipsis().child(truncated(project_name)))
                    .child(icon(
                        if self.picker_open {
                            IconName::CaretUp
                        } else {
                            IconName::CaretDown
                        },
                        px(16.),
                        t.muted,
                    )),
            )
            .when(self.picker_open, |this| {
                this.child(self.render_projects(cx))
            })
            // Channels. Virtualized: a project can collect hundreds of issue channels.
            .child(
                // Grows with the window, so a tall window shows more channels.
                section()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(140.))
                    .child(
                        uniform_list(
                            "channels",
                            channel_count + 1,
                            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                let store = this.store.read(cx);
                                let selected = store.selected();
                                let channels = &store.workspace().channels;
                                let rows: Vec<(Channel, bool)> = range
                                    .clone()
                                    .filter_map(|ix| channels.get(ix))
                                    .map(|c| (c.clone(), c.id == selected))
                                    .collect();
                                let wants_new = range.end > channels.len();
                                let mut items: Vec<gpui::AnyElement> = rows
                                    .iter()
                                    .map(|(c, sel)| {
                                        this.render_channel(c, *sel, cx).into_any_element()
                                    })
                                    .collect();
                                if wants_new {
                                    items.push(new_channel_row(cx).into_any_element());
                                }
                                items
                            }),
                        )
                        .flex_1()
                        .min_h_0(),
                    )
                    .child(
                        pill("archived", cx)
                            .mt(px(6.))
                            .h(px(36.))
                            .gap(px(10.))
                            .px(px(12.))
                            .child(icon(IconName::Archive, px(18.), t.ink_soft))
                            .child(div().flex_1().ellipsis().child(truncated("Archived")))
                            .child(icon(IconName::CaretRight, px(16.), t.muted)),
                    ),
            )
            // Quick links
            .child(
                section()
                    .flex()
                    .gap(px(6.))
                    .children(quick_links.into_iter().enumerate().map(
                        |(ix, (label, brand, url))| {
                            pill(("quick-link", ix), cx)
                                .flex_1()
                                .h(px(40.))
                                .justify_center()
                                .tooltip(Tooltip::text(SharedString::from(label)))
                                .on_click(move |_, _, cx| cx.open_url(&url))
                                .child(icon(brand_icon(brand), px(19.), t.ink_soft))
                        },
                    )),
            )
            .child(
                pill("settings", cx)
                    .h(px(44.))
                    .gap(px(10.))
                    .px(px(14.))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.pane
                            .update(cx, |p, cx| p.open(PaneContent::Settings, window, cx));
                    }))
                    .child(icon(IconName::GearSix, px(19.), t.ink_soft))
                    .child("Settings"),
            )
    }
}

fn new_channel_row(cx: &Context<Sidebar>) -> impl IntoElement + use<> {
    let t = *cx.theme();
    div().h(px(CHANNEL_ROW)).pb(px(6.)).child(
        pill("new-channel", cx)
            .h(px(36.))
            .justify_center()
            .tooltip(Tooltip::text("New channel"))
            .child(icon(IconName::Plus, px(18.), t.ink_soft)),
    )
}
