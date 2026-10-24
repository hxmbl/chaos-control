//! Floating message box. With no backend yet, sending posts the message locally.

use gpui::{Context, Entity, Focusable, Subscription, Window, div, prelude::*, px};
use ui::{ActiveTheme, IconName, TextInput, TextInputEvent, Tooltip, icon};

use crate::store::{ChatStore, StoreEvent};

pub struct Composer {
    store: Entity<ChatStore>,
    input: Entity<TextInput>,
    _subscriptions: [Subscription; 2],
}

fn placeholder(store: &ChatStore) -> String {
    format!("Message #{}", store.selected_channel().name)
}

impl Composer {
    pub fn new(store: Entity<ChatStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new(placeholder(store.read(cx)), cx));
        window.focus(&input.focus_handle(cx));
        let subscriptions = [
            cx.subscribe(&input, |this, _, event, cx| match event {
                TextInputEvent::Submit => this.send(cx),
                TextInputEvent::Changed => cx.notify(),
            }),
            cx.subscribe(&store, |this, store, event, cx| {
                if let StoreEvent::Selected = event {
                    let text = placeholder(store.read(cx));
                    this.input
                        .update(cx, |input, cx| input.set_placeholder(text, cx));
                }
            }),
        ];
        Self {
            store,
            input,
            _subscriptions: subscriptions,
        }
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().clone();
        if text.trim().is_empty() {
            return;
        }
        self.store
            .update(cx, |store, cx| store.post_from_human(&text, cx));
        self.input.update(cx, |input, cx| input.clear(cx));
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let can_send = !self.input.read(cx).text().trim().is_empty();
        let focused = self.input.focus_handle(cx).contains_focused(window, cx);
        let hover = t.hover;

        div()
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(22.))
            .border_1()
            .border_color(if focused { t.line_strong } else { t.line })
            .bg(t.surface)
            .px(px(8.))
            .pt(px(10.))
            .pb(px(8.))
            .shadow(t.float_shadow())
            .child(
                div()
                    .px(px(8.))
                    .text_size(px(15.))
                    .line_height(px(24.))
                    .text_color(t.ink)
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .mt(px(6.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .id("attach")
                            .size(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .hover(move |s| s.bg(hover))
                            .tooltip(Tooltip::text("Attach"))
                            .child(icon(IconName::Plus, px(18.), t.muted)),
                    )
                    .child(
                        div()
                            .id("send")
                            .size(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(t.ink)
                            .when(!can_send, |s| s.opacity(0.15))
                            .tooltip(Tooltip::text("Send"))
                            .on_click(cx.listener(|this, _, _, cx| this.send(cx)))
                            .child(icon(IconName::ArrowUpBold, px(16.), t.surface)),
                    ),
            )
    }
}
