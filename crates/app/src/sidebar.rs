//! Floating, detached left sidebar: the cross-project chat list, quick links,
//! settings.
//!
//! STRM-737 replaced "the open project's channels" with a chat list that spans
//! every project core knows. Each row is one chat (a channel) annotated with
//! its project badge and name; the open project's chats sit under **Active**,
//! the rest under **Settled**. Opening a chat from another project switches the
//! window to it without changing either project's folder identity.

use std::sync::Arc;

use gpui::{Context, Entity, SharedString, Window, div, prelude::*, px};
use model::{ChatEntry, ChatProject, PaneContent};
use ui::{
    ActiveTheme, Elevation, Ellipsis, IconName, Tooltip, TypeStyleExt, brand_icon, icon, pill,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::{pane_host::PaneHost, store::ChatStore};

/// One chat row: room for the name and the project caption, plus the gap
/// below it.
const CHAT_ROW: f32 = 48.;
/// A section label ("Active", "Settled") or a quiet empty row.
const SECTION_ROW: f32 = 32.;

/// Stable row id for a chat: channel numbers repeat in every project, so the
/// project id has to be part of the key or two `#main`s would share one
/// element id.
fn row_key(project: &str, channel: Option<model::Id>) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let bytes = project
        .bytes()
        .chain(channel.into_iter().flat_map(|c| c.0.to_le_bytes()));
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash as usize
}

pub struct Sidebar {
    store: Entity<ChatStore>,
    pane: Entity<PaneHost>,
}

impl Sidebar {
    pub fn new(store: Entity<ChatStore>, pane: Entity<PaneHost>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self { store, pane }
    }

    fn section_label(label: &'static str, cx: &Context<Self>) -> impl IntoElement + use<> {
        let t = *cx.theme();
        div()
            .h(px(SECTION_ROW))
            .flex()
            .items_end()
            .pb(space::X1_5)
            .px(space::X3)
            .child(
                div()
                    .type_style(text::CAPTION)
                    .font_weight(text::MEDIUM)
                    .text_color(t.muted)
                    .child(label),
            )
    }

    fn project_badge(mark: Arc<str>, cx: &Context<Self>) -> impl IntoElement + use<> {
        let t = *cx.theme();
        div()
            .size(px(18.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(radius::SM)
            .bg(t.sunken)
            .child(
                div()
                    .type_style(text::CAPTION)
                    .font_weight(text::SEMIBOLD)
                    .text_color(t.ink_soft)
                    .child(SharedString::from(mark)),
            )
    }

    /// One chat: its project badge, the channel name and the project it lives
    /// in. Unread is a dot on the right, never colour text (colour means
    /// status).
    fn render_chat(
        &self,
        project: &ChatProject,
        chat: &ChatEntry,
        selected: bool,
        can_open: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let t = *cx.theme();
        let channel = chat.channel;
        let project_id = project.id.clone();
        let key = row_key(&project.id, Some(channel));
        let mark = project.mark.clone();
        let name = SharedString::from(chat.name.clone());
        let project_name = SharedString::from(project.name.clone());
        let tooltip = match (&project.path, chat.ordinal) {
            (Some(path), Some(ordinal)) => {
                format!("{} in {} ::{ordinal}", project.name, path)
            }
            (Some(path), None) => format!("{} in {}", project.name, path),
            (None, _) => format!("{} (no folder)", project.name),
        };
        let tooltip = if can_open {
            tooltip
        } else {
            match &project.path {
                Some(path) => format!("Launch Chaos Control in {path} to open it"),
                None => "Not linked to a folder on this computer".to_string(),
            }
        };

        div().h(px(CHAT_ROW)).pb(space::X1).child(
            pill(("chat", key), cx)
                .h(size::ROW_LG)
                .gap(space::X2)
                .pl(space::X3)
                .pr(space::X2)
                .when(selected, |p| p.bg(t.selected).font_weight(text::MEDIUM))
                .tooltip(Tooltip::text(tooltip))
                .when(can_open, |p| {
                    p.on_click(cx.listener(move |this, _, _, cx| {
                        this.store.update(cx, |store, cx| {
                            store.open_chat(project_id.clone(), Some(channel), cx)
                        });
                    }))
                })
                .child(Self::project_badge(mark, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .ellipsis()
                                .type_style(text::SMALL)
                                .when(chat.unread, |d| d.font_weight(text::SEMIBOLD))
                                .child(truncated(name)),
                        )
                        .child(
                            div()
                                .ellipsis()
                                .type_style(text::CAPTION)
                                .text_color(t.muted)
                                .child(truncated(project_name)),
                        ),
                )
                .when(chat.unread, |p| {
                    p.child(
                        div()
                            .size(space::X1_5)
                            .flex_none()
                            .rounded_full()
                            .bg(t.accent),
                    )
                }),
        )
    }

    /// A project with no channels: still a way to open it, since
    /// `view_project` gives it a #main if it is somehow missing.
    fn render_empty_project(
        &self,
        project: &ChatProject,
        can_open: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let t = *cx.theme();
        let project_id = project.id.clone();
        let key = row_key(&project.id, None);
        let mark = project.mark.clone();
        let project_name = SharedString::from(project.name.clone());
        let tooltip = if can_open {
            "Open this project".to_string()
        } else {
            match &project.path {
                Some(path) => format!("Launch Chaos Control in {path} to open it"),
                None => "Not linked to a folder on this computer".to_string(),
            }
        };
        div().h(px(CHAT_ROW)).pb(space::X1).child(
            pill(("empty-project", key), cx)
                .h(size::ROW_LG)
                .gap(space::X2)
                .px(space::X3)
                .tooltip(Tooltip::text(tooltip))
                .when(can_open, |p| {
                    p.on_click(cx.listener(move |this, _, _, cx| {
                        this.store.update(cx, |store, cx| {
                            store.open_chat(project_id.clone(), None, cx)
                        });
                    }))
                })
                .child(Self::project_badge(mark, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .ellipsis()
                        .type_style(text::SMALL)
                        .text_color(t.muted)
                        .child(truncated(project_name)),
                )
                .child(icon(IconName::CaretRight, icons::SM, t.muted)),
        )
    }
}

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        // Clone the small catalogue so the store borrow ends before the rows
        // borrow `cx` mutably (listeners and tooltips).
        let (catalog, selected, project_count, can_open, has_source) = {
            let store = self.store.read(cx);
            (
                store.chats().to_vec(),
                store.selected(),
                store.chats().len(),
                store.has_core(),
                store.has_chat_source(),
            )
        };
        let quick_links: Vec<_> = self
            .store
            .read(cx)
            .workspace()
            .quick_links
            .iter()
            .map(|l| (l.label.clone(), l.brand, l.url.clone()))
            .collect();

        let current: Vec<&ChatProject> = catalog.iter().filter(|p| p.current).collect();
        let others: Vec<&ChatProject> = catalog.iter().filter(|p| !p.current).collect();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();

        if catalog.is_empty() {
            rows.push(
                div()
                    .px(space::X3)
                    .py(space::X2)
                    .type_style(text::SMALL)
                    .text_color(t.muted)
                    .child(if has_source {
                        "Loading chats…"
                    } else {
                        "No projects yet"
                    })
                    .into_any_element(),
            );
        }
        if !current.is_empty() {
            rows.push(Self::section_label("Active", cx).into_any_element());
            for project in &current {
                if project.chats.is_empty() {
                    rows.push(
                        self.render_empty_project(project, can_open, cx)
                            .into_any_element(),
                    );
                }
                for chat in &project.chats {
                    let is_selected = project.current && chat.channel == selected;
                    rows.push(
                        self.render_chat(project, chat, is_selected, can_open, cx)
                            .into_any_element(),
                    );
                }
            }
        }
        if !others.is_empty() {
            rows.push(
                Self::section_label(
                    if current.is_empty() {
                        "Projects"
                    } else {
                        "Settled"
                    },
                    cx,
                )
                .into_any_element(),
            );
            for project in &others {
                if project.chats.is_empty() {
                    rows.push(
                        self.render_empty_project(project, can_open, cx)
                            .into_any_element(),
                    );
                }
                for chat in &project.chats {
                    rows.push(
                        self.render_chat(project, chat, false, can_open, cx)
                            .into_any_element(),
                    );
                }
            }
        }
        if !can_open && !catalog.is_empty() {
            rows.push(
                div()
                    .px(space::X3)
                    .py(space::X2)
                    .type_style(text::CAPTION)
                    .text_color(t.muted)
                    .child("Launch Chaos Control in a project's folder to open it.")
                    .into_any_element(),
            );
        }

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
            // Chats: the workspace's cross-project list.
            .child(
                div()
                    .flex_none()
                    .h(size::ROW_LG)
                    .flex()
                    .items_center()
                    .gap(space::X2)
                    .px(space::X3)
                    .child(icon(IconName::ChatCircle, icons::MD, t.ink_soft))
                    .child(
                        div()
                            .flex_1()
                            .type_style(text::SMALL)
                            .font_weight(text::SEMIBOLD)
                            .child("Chats"),
                    )
                    .child(
                        div()
                            .type_style(text::CAPTION)
                            .text_color(t.muted)
                            .child(project_count.to_string()),
                    )
                    .child(
                        div()
                            .id("refresh-chats")
                            .size(size::BUTTON_SM)
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius::MD)
                            .cursor_pointer()
                            .hover(|s| s.bg(t.hover))
                            .tooltip(Tooltip::text("Refresh chats"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store.update(cx, |store, cx| store.refresh_chats(cx));
                            }))
                            .child(icon(IconName::ArrowClockwise, icons::SM, t.muted)),
                    ),
            )
            .child(
                div()
                    .id("chats-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(rows),
            )
            .child(
                pill("archived", cx)
                    .flex_none()
                    .h(size::ROW)
                    .gap(space::X2)
                    .pl(space::X3)
                    .pr(space::X3)
                    .child(icon(IconName::Archive, icons::MD, t.ink_soft))
                    .child(div().flex_1().ellipsis().child(truncated("Archived")))
                    .child(icon(IconName::CaretRight, icons::SM, t.muted)),
            )
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
                    .flex_none()
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
