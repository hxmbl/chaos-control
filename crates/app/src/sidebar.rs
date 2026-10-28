//! Floating, detached left sidebar: project picker, channels, quick links, settings.

use gpui::{Context, Entity, SharedString, Window, div, prelude::*, px, uniform_list};
use model::{Channel, PaneContent};
use ui::{
    ActiveTheme, Elevation, Ellipsis, IconName, Tooltip, TypeStyleExt, brand_icon, icon, pill,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::{pane_host::PaneHost, store::ChatStore};

/// Channel row height (`size::ROW`) plus the gap below it (`space::X1`);
/// `uniform_list` needs one fixed row height.
const CHANNEL_ROW: f32 = 36. + 4.;
/// The channel list never shrinks below about three rows (or its content, if
/// that is shorter).
const CHANNELS_MIN_H: f32 = 3. * CHANNEL_ROW;

/// The section's padding (`space::X1_5` twice) plus the Archived row (`size::ROW`).
const SECTION_CHROME: f32 = 2. * 6. + 36.;

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
            .gap(space::X1)
            .p(space::X1_5)
            .rounded(radius::LG)
            .bg(t.sunken)
            .children(entries.into_iter().enumerate().map(|(ix, entry)| {
                let path = entry.path.clone();
                pill(("project-entry", ix), cx)
                    .h(size::ROW_LG)
                    .px(space::X3)
                    .flex_col()
                    .items_start()
                    .justify_center()
                    .when(entry.current, |p| {
                        p.bg(t.selected).font_weight(text::MEDIUM)
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
                            .type_style(text::SMALL)
                            .child(truncated(SharedString::from(entry.name.clone()))),
                    )
                    .child(
                        div()
                            .w_full()
                            .ellipsis()
                            .type_style(text::CAPTION)
                            .font_weight(text::REGULAR)
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

        // Selected: a filled row in medium weight. Unread: semibold plus the
        // dot. One hash glyph throughout.
        div().h(px(CHANNEL_ROW)).pb(space::X1).child(
            pill(("channel", id.0 as usize), cx)
                .h(size::ROW)
                .gap(space::X2)
                .pl(space::X3)
                .pr(space::X2)
                .when(selected, |p| p.bg(t.selected).font_weight(text::MEDIUM))
                .when(channel.unread, |p| p.font_weight(text::SEMIBOLD))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.store.update(cx, |store, cx| store.select(id, cx));
                }))
                .child(icon(
                    IconName::Hash,
                    icons::MD,
                    if selected { t.ink } else { t.ink_soft },
                ))
                .child(div().flex_1().ellipsis().child(truncated(name)))
                .when(channel.unread, |p| {
                    p.child(
                        div()
                            .size(space::X1_5)
                            .flex_none()
                            .rounded_full()
                            .bg(t.accent),
                    )
                })
                .when_some(channel.linear_issue.clone(), |p, issue| {
                    p.child(
                        div()
                            .id(("linear", id.0 as usize))
                            .size(icons::LG)
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .tooltip(Tooltip::text(format!("Linked to Linear issue {issue}")))
                            .child(icon(IconName::BrandLinear, icons::SM, t.muted)),
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
        // Every channel plus the "+" row.
        let list_h = (channel_count + 1) as f32 * CHANNEL_ROW;
        let quick_links: Vec<_> = ws
            .quick_links
            .iter()
            .map(|l| (l.label.clone(), l.brand, l.url.clone()))
            .collect();

        // A section is a background step only: no outline inside the panel's.
        let section = || div().rounded(radius::LG).bg(t.sunken).p(space::X1_5);

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
            // Project picker
            .child(
                pill("project", cx)
                    .h(size::ROW_LG)
                    .gap(space::X2)
                    .px(space::X3)
                    .font_weight(text::SEMIBOLD)
                    .tooltip(Tooltip::text("Projects"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picker_open = !this.picker_open;
                        cx.notify();
                    }))
                    .child(div().type_style(text::BODY).child(project_emoji))
                    .child(div().flex_1().ellipsis().child(truncated(project_name)))
                    .child(icon(
                        if self.picker_open {
                            IconName::CaretUp
                        } else {
                            IconName::CaretDown
                        },
                        icons::SM,
                        t.muted,
                    )),
            )
            .when(self.picker_open, |this| {
                this.child(self.render_projects(cx))
            })
            // Channels. Virtualized: a project can collect hundreds of issue channels.
            // The section hugs its rows (channels, "+", then Archived), so it
            // grows with every new channel; once it would run into the quick
            // links it stops at the space left and the list scrolls inside it.
            // Archived stays at the section's end.
            .child(
                section()
                    .flex()
                    .flex_col()
                    .flex_shrink()
                    // The section's padding plus Archived below the list.
                    .min_h(px(list_h.min(CHANNELS_MIN_H) + SECTION_CHROME))
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
                        .h(px(list_h))
                        .flex_shrink()
                        .min_h(px(list_h.min(CHANNELS_MIN_H))),
                    )
                    .child(
                        pill("archived", cx)
                            .h(size::ROW)
                            .gap(space::X2)
                            .pl(space::X3)
                            .pr(space::X3)
                            .child(icon(IconName::Archive, icons::MD, t.ink_soft))
                            .child(div().flex_1().ellipsis().child(truncated("Archived")))
                            .child(icon(IconName::CaretRight, icons::SM, t.muted)),
                    ),
            )
            // Spare height sits under the channels, above the quick links.
            .child(div().flex_1())
            // Quick links: straight on the panel surface, hover only.
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(space::X1)
                    .px(space::X1_5)
                    .children(quick_links.into_iter().enumerate().map(
                        |(ix, (label, brand, url))| {
                            pill(("quick-link", ix), cx)
                                .flex_1()
                                .h(size::ROW_LG)
                                .justify_center()
                                .tooltip(Tooltip::text(SharedString::from(label)))
                                .on_click(move |_, _, cx| cx.open_url(&url))
                                .child(icon(brand_icon(brand), icons::LG, t.ink_soft))
                        },
                    )),
            )
            .child(
                pill("settings", cx)
                    .h(size::ROW_LG)
                    .gap(space::X2)
                    .px(space::X3)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.pane
                            .update(cx, |p, cx| p.open(PaneContent::Settings, window, cx));
                    }))
                    .child(icon(IconName::GearSix, icons::MD, t.ink_soft))
                    .child("Settings"),
            )
    }
}

fn new_channel_row(cx: &Context<Sidebar>) -> impl IntoElement + use<> {
    let t = *cx.theme();
    div().h(px(CHANNEL_ROW)).pb(space::X1).child(
        pill("new-channel", cx)
            .h(size::ROW)
            .justify_center()
            .tooltip(Tooltip::text("New channel"))
            .child(icon(IconName::Plus, icons::MD, t.ink_soft)),
    )
}
