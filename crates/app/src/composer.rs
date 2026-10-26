//! Floating message box. With no backend yet, sending posts the message locally.

use gpui::{
    Context, Entity, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px,
};
use model::{ReplyRef, pane::author_name};
use ui::{ActiveTheme, Ellipsis, IconName, TextInput, TextInputEvent, Tooltip, icon, truncated};

use crate::store::{ChatStore, StoreEvent};

pub struct Composer {
    store: Entity<ChatStore>,
    input: Entity<TextInput>,
    /// "Reply" in the pane: the message the next send answers.
    reply: Option<ReplyRef>,
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
                    // A reply belongs to the channel it was started in.
                    this.reply = None;
                    let text = placeholder(store.read(cx));
                    this.input
                        .update(cx, |input, cx| input.set_placeholder(text, cx));
                }
            }),
        ];
        Self {
            store,
            input,
            reply: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.input.focus_handle(cx));
    }

    /// Starts a reply to a message (the pane's "Reply"): shows the reference
    /// above the field and hands the keyboard to the composer.
    pub fn set_reply(&mut self, reply: ReplyRef, window: &mut Window, cx: &mut Context<Self>) {
        self.reply = Some(reply);
        self.focus(window, cx);
        cx.notify();
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().clone();
        if text.trim().is_empty() {
            return;
        }
        let reply = self.reply.take();
        self.store
            .update(cx, |store, cx| store.post_from_human(&text, reply, cx));
        self.input.update(cx, |input, cx| input.clear(cx));
    }

    fn render_reply(&self, reply: &ReplyRef, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = *cx.theme();
        let who = SharedString::from(author_name(self.store.read(cx).workspace(), reply.author));
        let hover = t.hover;
        div()
            .mx(px(4.))
            .mb(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .pl(px(10.))
            .pr(px(4.))
            .py(px(4.))
            .rounded(px(12.))
            .bg(t.sunken)
            .border_l_2()
            .border_color(t.accent)
            .text_size(px(13.))
            .line_height(px(18.))
            .child(div().flex_none().text_color(t.muted).child("Replying to"))
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.ink)
                    .child(who),
            )
            .child(
                div()
                    .flex_1()
                    .ellipsis()
                    .text_color(t.ink_soft)
                    .child(truncated(reply.quote.clone())),
            )
            .child(
                div()
                    .id("cancel-reply")
                    .size(px(22.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .hover(move |s| s.bg(hover))
                    .tooltip(Tooltip::text("Cancel reply"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.reply = None;
                        this.focus(window, cx);
                        cx.notify();
                    }))
                    .child(icon(IconName::X, px(12.), t.muted)),
            )
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
            .when_some(self.reply.clone(), |d, reply| {
                d.child(self.render_reply(&reply, cx))
            })
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
