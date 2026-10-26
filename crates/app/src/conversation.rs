//! The selected channel's messages.
//!
//! History is shown through GPUI's `list`, which only lays out and paints the rows
//! in view (plus a little overdraw) and remembers measured row heights, so a
//! channel with 100k messages costs about the same per frame as one with 20.
//! Messages arrive whole; appending splices rows onto the end of the list state
//! without touching the heights already measured.

use gpui::{
    AnyElement, Context, Entity, FontWeight, ListAlignment, ListOffset, ListState, SharedString,
    Subscription, Window, div, list, prelude::*, px,
};
use model::{Author, Id, PaneContent, pane};
use ui::{ActiveTheme, Ellipsis, IconName, brand_icon, icon, truncated};

use crate::{
    pane_host::PaneHost,
    store::{ChatStore, StoreEvent},
};

pub struct Conversation {
    store: Entity<ChatStore>,
    pane: Entity<PaneHost>,
    channel: Id,
    list: ListState,
    /// Message picked in search, tinted until the channel changes.
    highlighted: Option<usize>,
    _store_events: Subscription,
}

impl Conversation {
    pub fn new(store: Entity<ChatStore>, pane: Entity<PaneHost>, cx: &mut Context<Self>) -> Self {
        let channel = store.read(cx).selected();
        let count = store.read(cx).log(channel).map_or(0, |l| l.len());
        let subscription = cx.subscribe(&store, |this, store, event, cx| match event {
            StoreEvent::Selected => {
                this.channel = store.read(cx).selected();
                let count = store.read(cx).log(this.channel).map_or(0, |l| l.len());
                // Bottom-aligned, so a reset lands on the newest message.
                this.list.reset(count);
                this.highlighted = None;
                cx.notify();
            }
            StoreEvent::Reveal { channel, index } if *channel == this.channel => {
                // Put a couple of messages of context above the hit. Works before
                // rows are measured, unlike scroll_to_reveal_item.
                this.list.scroll_to(ListOffset {
                    item_ix: index.saturating_sub(2),
                    offset_in_item: px(0.),
                });
                this.highlighted = Some(*index);
                cx.notify();
            }
            StoreEvent::Reveal { .. } => {}
            StoreEvent::Appended { channel, range } if *channel == this.channel => {
                let old = this.list.item_count();
                this.list.splice(old..old, range.len());
                cx.notify();
            }
            StoreEvent::Appended { .. } => {}
        });
        Self {
            store,
            pane,
            channel,
            list: ListState::new(count, ListAlignment::Bottom, px(600.)),
            highlighted: None,
            _store_events: subscription,
        }
    }

    fn render_message(&self, ix: usize, cx: &gpui::App) -> AnyElement {
        let t = *cx.theme();
        let store = self.store.read(cx);
        let Some(message) = store.log(self.channel).and_then(|l| l.get(ix)) else {
            return div().into_any_element();
        };
        let (avatar, name): (AnyElement, SharedString) = match message.author {
            Author::Agent(id) => match store.workspace().agent(id) {
                Some(agent) => (
                    icon(brand_icon(agent.brand), px(16.), t.ink).into_any_element(),
                    agent.name.clone().into(),
                ),
                None => (div().into_any_element(), "Agent".into()),
            },
            Author::Human => (
                div()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Hx")
                    .into_any_element(),
                "Hx".into(),
            ),
            Author::System => (
                div()
                    .text_size(px(11.))
                    .text_color(t.ink_soft)
                    .child("·")
                    .into_any_element(),
                "Chaos Control".into(),
            ),
        };

        div()
            .w_full()
            .flex()
            .justify_center()
            .child(
                div()
                    .w_full()
                    .flex()
                    .gap(px(12.))
                    .px(px(8.))
                    .py(px(8.))
                    .rounded(px(14.))
                    .when(self.highlighted == Some(ix), |row| row.bg(t.selected))
                    .child(
                        div()
                            .size(px(32.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .border_1()
                            .border_color(t.line)
                            .bg(t.surface)
                            .text_color(t.ink)
                            .child(avatar),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_baseline()
                                    .gap(px(8.))
                                    .min_w_0()
                                    .child(
                                        div()
                                            .flex_shrink()
                                            .ellipsis()
                                            .text_size(px(14.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(truncated(name)),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(12.))
                                            .text_color(t.muted)
                                            .child(clock(message.sent_at)),
                                    ),
                            )
                            .when_some(message.reply_to.as_ref(), |d, reply| {
                                d.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .min_w_0()
                                        .mt(px(2.))
                                        .pl(px(8.))
                                        .border_l_2()
                                        .border_color(t.line_strong)
                                        .text_size(px(13.))
                                        .line_height(px(18.))
                                        .text_color(t.muted)
                                        .child(
                                            div()
                                                .flex_none()
                                                .font_weight(FontWeight::MEDIUM)
                                                .child(SharedString::from(pane::author_name(
                                                    store.workspace(),
                                                    reply.author,
                                                ))),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .ellipsis()
                                                .child(truncated(reply.quote.clone())),
                                        ),
                                )
                            })
                            .map(|d| {
                                if pane::is_long(&message.body) {
                                    d.child(self.render_collapsed(ix, message, cx))
                                } else {
                                    // Messages wrap; they are never clipped.
                                    d.child(
                                        div()
                                            .w_full()
                                            .min_w_0()
                                            .whitespace_normal()
                                            .text_color(t.ink_soft)
                                            .child(SharedString::from(message.body.clone())),
                                    )
                                }
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Conversation {
    /// A long message's first lines, ending in "…", with "Open" to read it in
    /// full in the pane (see `model::pane::is_long` for the threshold).
    fn render_collapsed(&self, ix: usize, message: &model::Message, cx: &gpui::App) -> AnyElement {
        let t = *cx.theme();
        let preview = pane::preview(&message.body);
        let lines = preview.lines().map(|line| {
            div()
                .w_full()
                .min_w_0()
                .whitespace_normal()
                .child(SharedString::from(line.to_string()))
        });
        let content = PaneContent::LongMessage {
            channel: self.channel,
            index: ix,
            message: message.clone(),
        };
        let pane = self.pane.clone();
        let words = message.body.split_whitespace().count();
        let hover = t.hover;
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(2.))
            .text_color(t.ink_soft)
            .children(lines)
            .child(
                div()
                    .mt(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .id(("open-message", ix))
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(6.))
                            .h(px(28.))
                            .px(px(12.))
                            .rounded_full()
                            .border_1()
                            .border_color(t.line_strong)
                            .bg(t.surface)
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.ink)
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .on_click(move |_, window, cx| {
                                pane.update(cx, |p, cx| p.open(content.clone(), window, cx));
                            })
                            .child("Open")
                            .child(icon(IconName::CaretRight, px(12.), t.muted)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(t.muted)
                            .child(SharedString::from(format!("{words} words"))),
                    ),
            )
            .into_any_element()
    }
}

/// "HH:MM" (UTC) without pulling in a date library. Good enough for sample data.
fn clock(secs: u64) -> SharedString {
    let minutes = secs / 60;
    format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60).into()
}

impl Render for Conversation {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let store = self.store.read(cx);
        let channel = store.selected_channel();

        if self.list.item_count() > 0 {
            let this = cx.entity().downgrade();
            return list(self.list.clone(), move |ix, _, cx| {
                this.upgrade()
                    .map(|view| view.read(cx).render_message(ix, cx))
                    .unwrap_or_else(|| div().into_any_element())
            })
            .size_full()
            .into_any_element();
        }

        let project = store.workspace().project.name.clone();
        let blurb: SharedString = if channel.project_wide {
            format!("The project-wide room. Everyone on {project}, human or AI, can talk here.")
                .into()
        } else if let Some(issue) = &channel.linear_issue {
            format!("Linked to {issue} on Linear. Nothing here yet.").into()
        } else {
            "Nothing here yet.".into()
        };

        // Empty state: no backend, no agents yet.
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .px(px(24.))
            .child(
                div()
                    .max_w(px(384.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .text_center()
                    .child(
                        // flex_none: the blurb below is measured unwrapped first,
                        // so the column can come out short and squash its items.
                        div()
                            .size(px(40.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .border_1()
                            .border_color(t.line)
                            .child(icon(IconName::Hash, px(18.), t.faint)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.ink_soft)
                            .child(SharedString::from(channel.name.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .line_height(px(20.))
                            .text_color(t.muted)
                            .child(blurb),
                    ),
            )
            .into_any_element()
    }
}
