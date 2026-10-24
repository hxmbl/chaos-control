//! The selected channel's messages.
//!
//! History is shown through GPUI's `list`, which only lays out and paints the rows
//! in view (plus a little overdraw) and remembers measured row heights, so a
//! channel with 100k messages costs about the same per frame as one with 20.
//! Messages arrive whole; appending splices rows onto the end of the list state
//! without touching the heights already measured.

use gpui::{
    AnyElement, Context, Entity, FontWeight, ListAlignment, ListState, SharedString, Subscription,
    Window, div, list, prelude::*, px,
};
use model::{Author, Id};
use ui::{ActiveTheme, Ellipsis, IconName, brand_icon, icon};

use crate::store::{ChatStore, StoreEvent};

pub struct Conversation {
    store: Entity<ChatStore>,
    channel: Id,
    list: ListState,
    _store_events: Subscription,
}

impl Conversation {
    pub fn new(store: Entity<ChatStore>, cx: &mut Context<Self>) -> Self {
        let channel = store.read(cx).selected();
        let count = store.read(cx).log(channel).map_or(0, |l| l.len());
        let subscription = cx.subscribe(&store, |this, store, event, cx| match event {
            StoreEvent::Selected => {
                this.channel = store.read(cx).selected();
                let count = store.read(cx).log(this.channel).map_or(0, |l| l.len());
                // Bottom-aligned, so a reset lands on the newest message.
                this.list.reset(count);
                cx.notify();
            }
            StoreEvent::Appended { channel, range } if *channel == this.channel => {
                let old = this.list.item_count();
                this.list.splice(old..old, range.len());
                cx.notify();
            }
            StoreEvent::Appended { .. } => {}
        });
        Self {
            store,
            channel,
            list: ListState::new(count, ListAlignment::Bottom, px(600.)),
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
                                            .child(name),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(12.))
                                            .text_color(t.muted)
                                            .child(clock(message.sent_at)),
                                    ),
                            )
                            .child(
                                // Long messages wrap; they are never clipped.
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .whitespace_normal()
                                    .text_color(t.ink_soft)
                                    .child(SharedString::from(message.body.clone())),
                            ),
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
                        div()
                            .size(px(40.))
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
                            .max_w_full()
                            .ellipsis()
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
