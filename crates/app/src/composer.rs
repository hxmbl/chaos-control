//! Floating message box. Sends to core in #main; in channels core isn't
//! wired to yet, sending is off and a note says so.

use std::{collections::HashMap, ops::Range};

use gpui::{
    Context, Entity, Focusable, KeyBinding, SharedString, Subscription, Window, actions, div,
    prelude::*,
};
use model::{Id, ReplyRef, pane::author_name};
use ui::{
    ActiveTheme, Elevation, Ellipsis, IconName, TextInput, TextInputEvent, Tooltip, TypeStyleExt,
    icon,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::store::{ChatStore, SendRoute, StoreEvent, UNWIRED_NOTE};

actions!(composer, [Cancel]);

/// Key context of the composer, around its text field.
pub const KEY_CONTEXT: &str = "Composer";

/// The composer's own keys. Enter and shift-enter are the text field's (see
/// `ui::TextInput::multiline`); Esc backs out of whatever the composer is in
/// the middle of, never the typed text.
pub fn bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new("escape", Cancel, Some(KEY_CONTEXT))]
}

/// What Esc does in the composer, innermost first.
#[derive(Debug, PartialEq, Eq)]
enum Escape {
    /// Close an open suggestion popup (@-mentions) and keep the reply.
    DismissSuggestions,
    /// Drop the reply reference. The typed text stays.
    CancelReply,
    /// Nothing to back out of: let Esc go on to whatever is behind.
    Propagate,
}

/// Esc precedence (STRM-576): a suggestion popup first, then the reply.
/// Edit mode (Up to edit your last message) slots in after the popup when it
/// exists. Text is never cleared.
fn escape(suggestions_open: bool, replying: bool) -> Escape {
    if suggestions_open {
        Escape::DismissSuggestions
    } else if replying {
        Escape::CancelReply
    } else {
        Escape::Propagate
    }
}

/// Whether Enter (or the send button) should post `text`: an empty or
/// whitespace-only composer sends nothing.
fn sendable(text: &str) -> bool {
    !text.trim().is_empty()
}

#[derive(Clone)]
struct Draft {
    text: SharedString,
    selection: Range<usize>,
    reversed: bool,
    reply: Option<ReplyRef>,
}

impl Draft {
    fn empty() -> Self {
        Self {
            text: SharedString::default(),
            selection: 0..0,
            reversed: false,
            reply: None,
        }
    }
}

pub struct Composer {
    store: Entity<ChatStore>,
    input: Entity<TextInput>,
    /// The channel the composer is showing a draft for.
    channel: Id,
    /// "Reply" in the pane: the message the next send answers.
    reply: Option<ReplyRef>,
    drafts: HashMap<Id, Draft>,
    /// A core send in flight: restored if the send fails.
    pending: Option<(Id, Draft)>,
    _subscriptions: [Subscription; 2],
}

/// Rows the composer grows to before it scrolls.
const MAX_LINES: usize = 8;

fn placeholder(store: &ChatStore) -> String {
    format!("Message #{}", store.selected_channel().name)
}

impl Composer {
    pub fn new(store: Entity<ChatStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Enter sends, shift-enter starts a new line, and the field grows with
        // its text (see `TextInput::multiline`).
        let input =
            cx.new(|cx| TextInput::new(placeholder(store.read(cx)), cx).multiline(MAX_LINES));
        window.focus(&input.focus_handle(cx));
        let channel = store.read(cx).selected();
        let subscriptions = [
            cx.subscribe(&input, |this, _, event, cx| match event {
                TextInputEvent::Submit => this.send(cx),
                TextInputEvent::Changed => {
                    if let Some((ch, draft)) = &this.pending {
                        if *ch == this.channel
                            && this.input.read(cx).text().as_ref() != draft.text.as_ref()
                        {
                            this.pending = None;
                        }
                    }
                    cx.notify()
                }
            }),
            cx.subscribe(&store, |this, store, event, cx| match event {
                StoreEvent::Selected => {
                    let next = store.read(cx).selected();
                    this.switch_channel(next, store, cx);
                }
                StoreEvent::SendFailed { channel, .. } => {
                    if this.pending.as_ref().is_some_and(|(ch, _)| ch == channel) {
                        let (_, draft) = this.pending.take().unwrap();
                        // Keep the text if the send failed.
                        this.drafts.insert(*channel, draft.clone());
                        if this.channel == *channel {
                            this.reply = draft.reply.clone();
                            this.input.update(cx, |input, cx| {
                                input.restore_draft_state(
                                    draft.text,
                                    draft.selection,
                                    draft.reversed,
                                    cx,
                                );
                            });
                            cx.notify();
                        }
                    }
                }
                StoreEvent::Appended { channel, .. } => {
                    // A successful core send shows up as an appended message.
                    if let Some((ch, draft)) = this.pending.as_ref() {
                        if ch == channel {
                            let sent = store
                                .read(cx)
                                .log(*channel)
                                .and_then(|log| log.get(log.len().saturating_sub(1)))
                                .is_some_and(|m| {
                                    matches!(m.author, model::Author::Human)
                                        && m.body.as_ref() == draft.text.as_ref()
                                });
                            if sent {
                                this.pending = None;
                                if this.channel == *channel {
                                    this.input.update(cx, |input, cx| input.clear(cx));
                                    this.reply = None;
                                    this.drafts.remove(channel);
                                    cx.notify();
                                } else {
                                    this.drafts.remove(channel);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }),
        ];
        Self {
            store,
            input,
            channel,
            reply: None,
            drafts: HashMap::new(),
            pending: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.input.focus_handle(cx));
    }

    fn snapshot(&self, cx: &Context<Self>) -> Draft {
        let (text, selection, reversed) = self.input.read(cx).draft_state();
        Draft {
            text,
            selection,
            reversed,
            reply: self.reply.clone(),
        }
    }

    fn is_empty_draft(draft: &Draft) -> bool {
        draft.text.as_ref().trim().is_empty() && draft.reply.is_none()
    }

    fn save_current(&mut self, cx: &Context<Self>) {
        let draft = self.snapshot(cx);
        if Self::is_empty_draft(&draft) {
            self.drafts.remove(&self.channel);
        } else {
            self.drafts.insert(self.channel, draft);
        }
    }

    fn restore_for(&mut self, channel: Id, cx: &mut Context<Self>) {
        let draft = self
            .drafts
            .get(&channel)
            .cloned()
            .unwrap_or_else(Draft::empty);
        self.reply = draft.reply;
        self.input.update(cx, |input, cx| {
            input.restore_draft_state(draft.text, draft.selection, draft.reversed, cx);
        });
    }

    fn switch_channel(&mut self, next: Id, store: Entity<ChatStore>, cx: &mut Context<Self>) {
        if next == self.channel {
            return;
        }
        self.save_current(cx);
        self.channel = next;
        let placeholder = placeholder(store.read(cx));
        self.input
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        self.restore_for(next, cx);
        cx.notify();
    }

    /// Starts a reply to a message (the pane's "Reply"): shows the reference
    /// above the field and hands the keyboard to the composer.
    pub fn set_reply(&mut self, reply: ReplyRef, window: &mut Window, cx: &mut Context<Self>) {
        self.reply = Some(reply);
        self.focus(window, cx);
        cx.notify();
    }

    /// Hook for the @-mention popup (STRM-543): true while it is showing,
    /// so Esc closes it before touching the reply. No popup exists yet.
    fn suggestions_open(&self) -> bool {
        false
    }

    /// Closes the @-mention popup. Nothing to close until it exists.
    fn dismiss_suggestions(&mut self, _cx: &mut Context<Self>) {}

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        match escape(self.suggestions_open(), self.reply.is_some()) {
            Escape::DismissSuggestions => self.dismiss_suggestions(cx),
            Escape::CancelReply => {
                self.reply = None;
                self.save_current(cx);
                cx.notify();
            }
            Escape::Propagate => cx.propagate(),
        }
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().clone();
        if !sendable(&text) || self.unwired(cx) {
            return;
        }
        let route = self.store.read(cx).send_route();
        let reply = self.reply.take();
        // Keep a copy so a failed send can be restored.
        let (draft_text, selection, reversed) = self.input.read(cx).draft_state();
        let recovery = Draft {
            text: draft_text,
            selection,
            reversed,
            reply: reply.clone(),
        };
        self.store
            .update(cx, |store, cx| store.post_from_human(&text, reply, cx));
        match route {
            SendRoute::Core => {
                // Keep the draft until the send is known to have failed; core
                // reports success by echoing the message back.
                self.pending = Some((self.channel, recovery));
            }
            _ => {
                self.input.update(cx, |input, cx| input.clear(cx));
                self.reply = None;
                self.drafts.remove(&self.channel);
            }
        }
        cx.notify();
    }

    /// The selected channel isn't wired to core, so sending is off.
    fn unwired(&self, cx: &Context<Self>) -> bool {
        self.store.read(cx).send_route() == SendRoute::Unwired
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
        let unwired = self.unwired(cx);
        let can_send = !unwired && sendable(self.input.read(cx).text());
        let focused = self.input.focus_handle(cx).contains_focused(window, cx);
        let hover = t.hover;

        div()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::cancel))
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
            .when(unwired, |d| {
                d.child(
                    div()
                        .px(space::X2)
                        .mb(space::X1_5)
                        .type_style(text::SMALL)
                        .text_color(t.muted)
                        .child(UNWIRED_NOTE),
                )
            })
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

#[cfg(test)]
mod tests {
    use gpui::{Action as _, KeyContext, Keymap, Keystroke};

    use super::*;

    /// The action `keys` runs with focus in the composer's text field, as
    /// GPUI's dispatcher would pick it from the whole app keymap.
    fn action_for(keys: &str) -> Option<&'static str> {
        let mut all = bindings();
        all.extend(ui::text_input::bindings());
        let keymap = Keymap::new(all);
        let contexts = [
            KeyContext::parse("Workspace").unwrap(),
            KeyContext::parse(KEY_CONTEXT).unwrap(),
            KeyContext::parse(ui::text_input::MULTILINE_KEY_CONTEXT).unwrap(),
        ];
        let keystroke = Keystroke::parse(keys).unwrap();
        let (matched, _) = keymap.bindings_for_input(&[keystroke], &contexts);
        matched.first().map(|binding| binding.action().name())
    }

    #[test]
    fn esc_in_the_composer_cancels() {
        assert_eq!(action_for("escape"), Some(Cancel.name()));
    }

    #[test]
    fn enter_and_shift_enter_still_reach_the_text_field() {
        assert_eq!(action_for("enter"), Some(ui::text_input::Submit.name()));
        assert_eq!(
            action_for("shift-enter"),
            Some(ui::text_input::Newline.name())
        );
    }

    #[test]
    fn esc_closes_suggestions_before_the_reply() {
        assert_eq!(escape(true, true), Escape::DismissSuggestions);
        assert_eq!(escape(true, false), Escape::DismissSuggestions);
    }

    #[test]
    fn esc_cancels_a_reply_when_nothing_is_open() {
        assert_eq!(escape(false, true), Escape::CancelReply);
    }

    #[test]
    fn esc_with_nothing_to_cancel_passes_through() {
        assert_eq!(escape(false, false), Escape::Propagate);
    }

    #[test]
    fn empty_or_whitespace_only_text_is_not_sent() {
        assert!(!sendable(""));
        assert!(!sendable("   "));
        assert!(!sendable("\n\n"));
        assert!(!sendable(" \t\n \r\n"));
        assert!(sendable("hi"));
        assert!(sendable("  hi\n"));
    }
}
