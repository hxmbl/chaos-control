//! Floating message box. With no backend yet, sending posts the message locally.

use gpui::{Context, Entity, Focusable, SharedString, Subscription, Window, div, prelude::*};
use model::{ReplyRef, pane::author_name};
use ui::{
    ActiveTheme, Elevation, Ellipsis, IconName, TextInput, TextInputEvent, Tooltip, TypeStyleExt,
    icon,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

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
            .mx(space::X1)
            .mb(space::X2)
            .flex()
            .items_center()
            .gap(space::X2)
            .pl(space::X3)
            .pr(space::X1)
            .py(space::X1)
            .rounded(radius::MD)
            .bg(t.sunken)
            .border_l_2()
            .border_color(t.accent)
            .type_style(text::SMALL)
            .child(div().flex_none().text_color(t.muted).child("Replying to"))
            .child(
                div()
                    .flex_none()
                    .font_weight(text::SEMIBOLD)
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
                    .size(size::CHIP)
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(radius::MD)
                    .hover(move |s| s.bg(hover))
                    .tooltip(Tooltip::text("Cancel reply"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.reply = None;
                        this.focus(window, cx);
                        cx.notify();
                    }))
                    .child(icon(IconName::X, icons::SM, t.muted)),
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
            .rounded(radius::FIELD)
            .border_1()
            .border_color(if focused { t.line_strong } else { t.line })
            .bg(t.surface)
            .px(space::X2)
            .pt(space::X3)
            .pb(space::X2)
            .shadow(t.elevation(Elevation::Float))
            .when_some(self.reply.clone(), |d, reply| {
                d.child(self.render_reply(&reply, cx))
            })
            .child(
                div()
                    .px(space::X2)
                    .type_style(text::BODY)
                    .text_color(t.ink)
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .mt(space::X1_5)
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .id("attach")
                            .size(size::BUTTON)
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius::MD)
                            .hover(move |s| s.bg(hover))
                            .tooltip(Tooltip::text("Attach"))
                            .child(icon(IconName::Plus, icons::MD, t.muted)),
                    )
                    .child(
                        div()
                            .id("send")
                            .size(size::BUTTON)
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius::MD)
                            // Disabled reads as a muted button, not a missing one.
                            .bg(if can_send { t.ink } else { t.faint })
                            .tooltip(Tooltip::text("Send"))
                            .on_click(cx.listener(|this, _, _, cx| this.send(cx)))
                            .child(icon(IconName::ArrowUp, icons::MD, t.surface)),
                    ),
            )
    }
}
