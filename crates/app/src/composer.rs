//! Floating message box. Sends to core in #main; in channels core isn't
//! wired to yet, sending is off and a note says so.

use std::{collections::HashMap, ops::Range, time::Duration};

use gpui::{
    Animation, AnimationExt, Context, Entity, Focusable, Hsla, KeyBinding, KeyDownEvent,
    SharedString, StyledText, Subscription, TextRun, TextStyle, Window, actions, div, linear,
    prelude::*,
};
use model::{Id, ReplyRef, pane::author_name};
use ui::{
    ActiveTheme, Elevation, Ellipsis, IconName, TextInput, TextInputEvent, Tooltip, TypeStyleExt,
    icon,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::mention_highlight::{self, KnownMentions};
use crate::store::{
    ChatStore, MentionCandidate, SendRoute, SlashCommandCandidate, SlashCommandInvocation,
    StoreEvent, UNWIRED_NOTE,
};

actions!(composer, [Cancel, AcceptSuggestion]);

/// Key context of the composer, around its text field.
pub const KEY_CONTEXT: &str = "Composer";

/// The composer's own keys. Enter and shift-enter are the text field's (see
/// `ui::TextInput::multiline`); Esc backs out of whatever the composer is in
/// the middle of, never the typed text. Tab accepts an open suggestion.
pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("escape", Cancel, Some(KEY_CONTEXT)),
        KeyBinding::new("tab", AcceptSuggestion, Some(KEY_CONTEXT)),
    ]
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

/// Core echoes the posted text trimmed; compare against that.
fn sent_body(text: &str) -> &str {
    text.trim()
}

fn working_indicator_text(
    label: &str,
    delta: f32,
    base_style: &TextStyle,
    base: Hsla,
    bright: Hsla,
) -> StyledText {
    let label: SharedString = label.to_string().into();
    let chars: Vec<(usize, char)> = label.char_indices().collect();
    if chars.is_empty() {
        return StyledText::new(label);
    }
    let n = chars.len() as f32;
    // Sweep center moves across the text (with a bit of lead-in/out).
    let center = -0.25 + delta * 1.5;
    let mut runs: Vec<TextRun> = Vec::with_capacity(chars.len());
    for (i, &(byte_ix, _ch)) in chars.iter().enumerate() {
        let next = chars.get(i + 1).map(|(ix, _)| *ix).unwrap_or(label.len());
        let len = next - byte_ix;
        let p = (i as f32 + 0.5) / n;
        let d = (p - center).abs();
        // Soft highlight band with falloff.
        let highlight = (1.0 - (d / 0.18).min(1.0)).powf(1.8);
        // Gentle global pulse so text smoothly fades in/out overall.
        let pulse = 0.55 + 0.45 * (1.0 - (2.0 * (delta - 0.5)).abs());
        let mix = (highlight * 0.85 + pulse * 0.15).clamp(0.0, 1.0);
        let mut color = base;
        color.h = base.h + (bright.h - base.h) * mix;
        color.s = base.s + (bright.s - base.s) * mix;
        color.l = base.l + (bright.l - base.l) * mix;
        color.a = (0.45 + 0.55 * mix).clamp(0.0, 1.0);
        runs.push(TextRun {
            len,
            font: base_style.font(),
            color,
            background_color: base_style.background_color,
            underline: base_style.underline,
            strikethrough: base_style.strikethrough,
        });
    }
    StyledText::new(label).with_runs(runs)
}

fn echo_matches_in(
    log: Option<&model::ChannelLog>,
    range: std::ops::Range<usize>,
    sent: &str,
) -> bool {
    let Some(log) = log else {
        return false;
    };
    range
        .filter_map(|ix| log.get(ix))
        .any(|m| matches!(m.author, model::Author::Human) && m.body.as_ref() == sent_body(sent))
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

    fn is_empty(&self) -> bool {
        self.text.as_ref().trim().is_empty() && self.reply.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MentionState {
    /// Byte range in the draft text to replace: `@` through the mention token.
    range: Range<usize>,
    /// Text typed after `@` up to the cursor, used for filtering.
    query: String,
    matches: Vec<MentionCandidate>,
    selected: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SlashCommandState {
    /// Byte range covering the leading slash command token/name (`/cmd`).
    range: Range<usize>,
    /// Text typed after `/` in the command name (up to cursor), for filtering.
    query: String,
    /// Full arguments text after the command token, if any (including leading space).
    arguments: String,
    matches: Vec<SlashCommandCandidate>,
    selected: usize,
}

fn mention_query(text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    let at = before.rfind('@')?;
    let prefix = &before[at + 1..];
    if prefix.chars().any(char::is_whitespace) {
        return None;
    }
    if at > 0 {
        let prev = before[..at].chars().last()?;
        if !(prev.is_whitespace() || "([{\"'".contains(prev)) {
            return None;
        }
    }
    let after = &text[cursor..];
    let suffix_len = after
        .chars()
        .take_while(|c| !c.is_whitespace())
        .map(char::len_utf8)
        .sum::<usize>();
    let end = cursor + suffix_len;
    Some((at..end, prefix.to_string()))
}

fn filter_mentions(candidates: &[MentionCandidate], query: &str) -> Vec<MentionCandidate> {
    let q = query.to_ascii_lowercase();
    candidates
        .iter()
        .filter(|c| {
            c.handle.to_ascii_lowercase().contains(&q)
                || c.name.to_ascii_lowercase().contains(&q)
                || c.nicknames
                    .iter()
                    .any(|n| n.to_ascii_lowercase().contains(&q))
        })
        .cloned()
        .collect()
}

/// Detects a slash command at the start of the draft when the cursor is within
/// the command name token. Returns `(range, query, arguments)` where `range` is
/// the `/name` token span, `query` is the typed name after `/`, and `arguments`
/// is the remainder of the draft after that token.
fn slash_command_query(text: &str, cursor: usize) -> Option<(Range<usize>, String, String)> {
    if !text.starts_with('/') {
        return None;
    }
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let ws = text.find(|c: char| c.is_whitespace()).unwrap_or(text.len());
    // Only suggest while editing the command name token (not arguments).
    if cursor > ws {
        return None;
    }
    let name = &text[1..ws];
    if name.chars().any(char::is_whitespace) {
        return None;
    }
    let query_end = cursor.max(1).min(ws);
    let query = text[1..query_end].to_string();
    let arguments = text[ws..].to_string();
    Some((0..ws, query, arguments))
}

fn filter_slash_commands(
    candidates: &[SlashCommandCandidate],
    query: &str,
) -> Vec<SlashCommandCandidate> {
    let q = query.to_ascii_lowercase();
    candidates
        .iter()
        .filter(|c| c.name.to_ascii_lowercase().contains(&q))
        .cloned()
        .collect()
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
    mention: Option<MentionState>,
    slash_command: Option<SlashCommandState>,
    _subscriptions: [Subscription; 4],
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
        {
            let store_for_highlight = store.clone();
            input.update(cx, |input, cx| {
                input.set_highlighter(
                    move |text, appearance, app| {
                        let store = store_for_highlight.read(app);
                        let theme = *app.theme();
                        let mut known = KnownMentions::new();
                        for member in store.mention_candidates() {
                            known.insert_member(
                                member.handle.as_ref(),
                                member.handle.as_ref(),
                                &member.nicknames,
                            );
                        }
                        known.insert_broadcasts();
                        let mentions =
                            mention_highlight::mention_highlights(text, &known, appearance);
                        let md = crate::markdown::markdown_marker_highlights(text, &theme);
                        gpui::combine_highlights(mentions, md).collect()
                    },
                    cx,
                );
            });
        }
        window.focus(&input.focus_handle(cx));
        let channel = store.read(cx).selected();
        let subscriptions = [
            cx.subscribe(&input, |this, _, event, cx| match event {
                TextInputEvent::Submit => {
                    if this.suggestions_open() {
                        this.accept_mention(cx);
                    } else {
                        this.send(cx);
                    }
                }
                TextInputEvent::Changed => this.refresh_suggestions(cx),
            }),
            cx.observe(&input, |this, _, cx| this.refresh_suggestions(cx)),
            cx.observe(&store, |_, _, cx| cx.notify()),
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
                        if this.channel == *channel && this.input.read(cx).text().is_empty() {
                            this.reply = draft.reply.clone();
                            this.input.update(cx, |input, cx| {
                                input.restore_draft_state(
                                    draft.text,
                                    draft.selection,
                                    draft.reversed,
                                    cx,
                                );
                            });
                        }
                        cx.notify();
                    }
                }
                StoreEvent::Appended { channel, range } => {
                    // A successful core send shows up as an appended message.
                    let matched = this.pending.as_ref().and_then(|(ch, draft)| {
                        (*ch == *channel
                            && echo_matches_in(
                                store.read(cx).log(*channel),
                                range.clone(),
                                draft.text.as_ref(),
                            ))
                        .then(|| draft.text.as_ref().to_string())
                    });
                    if let Some(sent_text) = matched {
                        this.pending = None;
                        this.drafts.remove(channel);
                        if this.channel == *channel {
                            if this.input.read(cx).text().as_ref() == sent_text.as_str() {
                                this.input.update(cx, |input, cx| input.clear(cx));
                                this.reply = None;
                            }
                            cx.notify();
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
            mention: None,
            slash_command: None,
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

    fn save_current(&mut self, cx: &Context<Self>) {
        let draft = self.snapshot(cx);
        if draft.is_empty() {
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
        self.mention = None;
        self.slash_command = None;
        self.sync_vertical_nav_suppression(cx);
        self.refresh_suggestions(cx);
        cx.notify();
    }

    /// Starts a reply to a message (the pane's "Reply"): shows the reference
    /// above the field and hands the keyboard to the composer.
    pub fn set_reply(&mut self, reply: ReplyRef, window: &mut Window, cx: &mut Context<Self>) {
        self.reply = Some(reply);
        self.focus(window, cx);
        cx.notify();
    }

    /// True while a suggestion popup (@-mention or slash command) owns
    /// keyboard selection.
    fn suggestions_open(&self) -> bool {
        self.mention.is_some() || self.slash_command.is_some()
    }

    fn sync_vertical_nav_suppression(&self, cx: &mut Context<Self>) {
        let open = self.suggestions_open();
        self.input.update(cx, |input, _| {
            input.set_suppress_vertical_navigation(open);
        });
    }

    /// Closes any open suggestion popup.
    fn dismiss_suggestions(&mut self, cx: &mut Context<Self>) {
        if self.mention.take().is_some() || self.slash_command.take().is_some() {
            self.sync_vertical_nav_suppression(cx);
            cx.notify();
        }
    }

    fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let (text, selection, _) = self.input.read(cx).draft_state();
        if selection.start != selection.end {
            let changed = self.mention.take().is_some() || self.slash_command.take().is_some();
            if changed {
                self.sync_vertical_nav_suppression(cx);
                cx.notify();
            }
            return;
        }
        let cursor = selection.end.min(text.len());
        // Slash-command suggestions take precedence when active in the
        // appropriate input context (start of draft/command name).
        if self.refresh_slash_command(text.as_ref(), cursor, cx) {
            return;
        }
        self.refresh_mention(text.as_ref(), cursor, cx);
    }

    /// Updates slash-command suggestion state. Returns true when slash-command
    /// handling owns suggestion state for this cursor position (including the
    /// case where a slash command context is active but has no matches).
    fn refresh_slash_command(&mut self, text: &str, cursor: usize, cx: &mut Context<Self>) -> bool {
        let Some((range, query, arguments)) = slash_command_query(text, cursor) else {
            if self.slash_command.take().is_some() {
                self.sync_vertical_nav_suppression(cx);
                cx.notify();
            }
            return false;
        };
        let candidates = self.store.read(cx).slash_command_candidates();
        let matches = filter_slash_commands(&candidates, &query);
        if matches.is_empty() {
            let changed = self.slash_command.take().is_some() || self.mention.take().is_some();
            if changed {
                self.sync_vertical_nav_suppression(cx);
                cx.notify();
            }
            return true;
        }
        if self.mention.take().is_some() {
            // Slash suggestions take precedence over mention suggestions.
        }
        let selected = self
            .slash_command
            .as_ref()
            .filter(|m| m.query == query)
            .and_then(|m| {
                m.matches
                    .get(m.selected)
                    .and_then(|keep| matches.iter().position(|c| c.name == keep.name))
            })
            .unwrap_or(0);
        let next = SlashCommandState {
            range,
            query,
            arguments,
            matches,
            selected,
        };
        if self.slash_command.as_ref() == Some(&next) {
            self.sync_vertical_nav_suppression(cx);
            return true;
        }
        self.slash_command = Some(next);
        self.sync_vertical_nav_suppression(cx);
        cx.notify();
        true
    }

    fn refresh_mention(&mut self, text: &str, cursor: usize, cx: &mut Context<Self>) {
        let Some((range, query)) = mention_query(text, cursor) else {
            if self.mention.take().is_some() {
                self.sync_vertical_nav_suppression(cx);
                cx.notify();
            }
            return;
        };
        let candidates = self.store.read(cx).mention_candidates();
        let matches = filter_mentions(&candidates, &query);
        if matches.is_empty() {
            if self.mention.take().is_some() {
                self.sync_vertical_nav_suppression(cx);
                cx.notify();
            }
            return;
        }
        let selected = self
            .mention
            .as_ref()
            .filter(|m| m.query == query)
            .and_then(|m| {
                m.matches
                    .get(m.selected)
                    .and_then(|keep| matches.iter().position(|c| c.handle == keep.handle))
            })
            .unwrap_or(0);
        let next = MentionState {
            range,
            query,
            matches,
            selected,
        };
        if self.mention.as_ref() == Some(&next) {
            self.sync_vertical_nav_suppression(cx);
            return;
        }
        self.mention = Some(next);
        self.sync_vertical_nav_suppression(cx);
        cx.notify();
    }

    fn move_suggestion_selection(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        if let Some(state) = &mut self.slash_command {
            let len = state.matches.len();
            if len == 0 {
                return false;
            }
            let cur = state.selected as isize;
            state.selected = (cur + delta).rem_euclid(len as isize) as usize;
            cx.notify();
            return true;
        }
        let Some(state) = &mut self.mention else {
            return false;
        };
        let len = state.matches.len();
        if len == 0 {
            return false;
        }
        let cur = state.selected as isize;
        state.selected = (cur + delta).rem_euclid(len as isize) as usize;
        cx.notify();
        true
    }

    fn accept_suggestion(&mut self, cx: &mut Context<Self>) -> bool {
        if self.slash_command.is_some() {
            return self.accept_slash_command(cx);
        }
        self.accept_mention(cx)
    }

    fn accept_slash_command(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.slash_command.clone() else {
            return false;
        };
        let Some(candidate) = state.matches.get(state.selected) else {
            self.slash_command = None;
            self.sync_vertical_nav_suppression(cx);
            return false;
        };
        let (text, selection, _reversed) = self.input.read(cx).draft_state();
        if selection.start != selection.end {
            self.refresh_suggestions(cx);
            return false;
        }
        let cursor = selection.end.min(text.len());
        if cursor > state.range.end || state.range.start != 0 || state.range.end > text.len() {
            self.refresh_suggestions(cx);
            return false;
        }
        // Keep arguments after the command name token and dispatch the invocation.
        let args = state.arguments.clone();
        let invocation = SlashCommandInvocation {
            name: candidate.name.clone(),
            arguments: args.trim_start().to_string(),
        };
        self.store
            .update(cx, |store, _cx| store.dispatch_slash_command(invocation));
        self.input.update(cx, |input, cx| input.clear(cx));
        self.reply = None;
        self.drafts.remove(&self.channel);
        self.slash_command = None;
        self.sync_vertical_nav_suppression(cx);
        self.refresh_suggestions(cx);
        true
    }

    fn accept_mention(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.mention.clone() else {
            return false;
        };
        let Some(candidate) = state.matches.get(state.selected) else {
            self.mention = None;
            self.sync_vertical_nav_suppression(cx);
            return false;
        };
        let (text, selection, _reversed) = self.input.read(cx).draft_state();
        if selection.start != selection.end {
            self.refresh_suggestions(cx);
            return false;
        }
        let cursor = selection.end.min(text.len());
        if cursor < state.range.start
            || cursor > state.range.end
            || state.range.end > text.len()
            || !text.is_char_boundary(state.range.start)
            || !text.is_char_boundary(state.range.end)
            || !text.is_char_boundary(cursor)
        {
            self.refresh_suggestions(cx);
            return false;
        }
        let handle = candidate.handle.to_string();
        let mut new = String::with_capacity(text.len() - state.range.len() + handle.len() + 2);
        new.push_str(&text.as_ref()[..state.range.start]);
        new.push('@');
        new.push_str(&handle);
        let insert_end = new.len();
        new.push(' ');
        new.push_str(&text.as_ref()[state.range.end..]);
        let caret = insert_end + 1;
        self.input.update(cx, |input, cx| {
            input.restore_draft_state(SharedString::from(new), caret..caret, false, cx);
        });
        self.mention = None;
        self.sync_vertical_nav_suppression(cx);
        self.refresh_suggestions(cx);
        true
    }

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

    fn accept_suggestion_action(
        &mut self,
        _: &AcceptSuggestion,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.accept_suggestion(cx) {
            cx.propagate();
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
        self.input.update(cx, |input, cx| input.clear(cx));
        self.reply = None;
        self.drafts.remove(&self.channel);
        if route == SendRoute::Core {
            // Keep the draft until the send is known to have failed; core
            // reports success by echoing the message back.
            self.pending = Some((self.channel, recovery));
        }
        cx.notify();
    }

    /// The selected channel isn't wired to core, so sending is off.
    fn unwired(&self, cx: &Context<Self>) -> bool {
        self.store.read(cx).send_route() == SendRoute::Unwired
    }

    fn working_indicator(&self, cx: &Context<Self>) -> Option<SharedString> {
        let store = self.store.read(cx);
        let agents = store.working_agents(store.selected());
        if agents.is_empty() {
            return None;
        }
        let label = if agents.len() == 1 {
            let (name, verb) = &agents[0];
            format!("{} is {verb}…", name)
        } else {
            let names = agents
                .iter()
                .map(|(name, _)| name.as_ref())
                .collect::<Vec<_>>();
            let joined = match names.as_slice() {
                [] => return None,
                [a] => (*a).to_string(),
                [a, b] => format!("{a} and {b}"),
                [all @ .., last] => format!("{}, and {last}", all.join(", ")),
            };
            format!("{joined} are Working…")
        };
        Some(label.into())
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
                        this.save_current(cx);
                        cx.notify();
                    }))
                    .child(icon(IconName::X, icons::SM, t.muted)),
            )
    }

    fn render_suggestions(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if let Some(state) = &self.slash_command {
            let t = *cx.theme();
            let hover = t.hover;
            let rows = state.matches.iter().enumerate().map(|(ix, c)| {
                let selected = ix == state.selected;
                let name = format!("/{}", c.name);
                let hint = c.argument_hint.as_ref().map(|h| h.to_string());
                let desc = c.description.to_string();
                let idx = ix;
                div()
                    .id(("slash-command", ix))
                    .px(space::X2)
                    .py(space::X1_5)
                    .rounded(radius::MD)
                    .bg(if selected { t.selected } else { t.surface })
                    .hover(move |s| s.bg(if selected { t.selected } else { hover }))
                    .cursor_pointer()
                    .flex()
                    .flex_col()
                    .gap(space::X0_5)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(state) = &mut this.slash_command {
                                state.selected = idx;
                            }
                            this.accept_suggestion(cx);
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::X2)
                            .child(
                                div()
                                    .type_style(text::SMALL)
                                    .font_weight(text::SEMIBOLD)
                                    .text_color(t.ink)
                                    .child(name),
                            )
                            .when_some(hint, |d, hint| {
                                d.child(
                                    div()
                                        .type_style(text::SMALL)
                                        .text_color(t.muted)
                                        .child(hint),
                                )
                            }),
                    )
                    .child(
                        div()
                            .type_style(text::CAPTION)
                            .text_color(t.muted)
                            .child(desc),
                    )
            });
            return Some(
                div()
                    .mx(space::X1)
                    .mb(space::X2)
                    .p(space::X1)
                    .rounded(radius::MD)
                    .bg(t.surface)
                    .border_1()
                    .border_color(t.line)
                    .shadow(t.elevation(Elevation::Overlay))
                    .flex()
                    .flex_col()
                    .gap(space::X0_5)
                    .children(rows)
                    .into_any_element(),
            );
        }

        let state = self.mention.as_ref()?;
        let t = *cx.theme();
        let hover = t.hover;
        let rows = state.matches.iter().enumerate().map(|(ix, m)| {
            let selected = ix == state.selected;
            let handle = format!("@{}", m.handle);
            let nick = if m.nicknames.is_empty() {
                None
            } else {
                Some(
                    m.nicknames
                        .iter()
                        .map(|n| format!("@{n}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            };
            let idx = ix;
            div()
                .id(("mention", ix))
                .px(space::X2)
                .py(space::X1_5)
                .rounded(radius::MD)
                .bg(if selected { t.selected } else { t.surface })
                .hover(move |s| s.bg(if selected { t.selected } else { hover }))
                .cursor_pointer()
                .flex()
                .flex_col()
                .gap(space::X0_5)
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        if let Some(state) = &mut this.mention {
                            state.selected = idx;
                        }
                        this.accept_suggestion(cx);
                    }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::X2)
                        .child(
                            div()
                                .type_style(text::SMALL)
                                .font_weight(text::SEMIBOLD)
                                .text_color(t.ink)
                                .child(handle),
                        )
                        .child(
                            div()
                                .type_style(text::SMALL)
                                .text_color(t.muted)
                                .child(SharedString::from(m.name.clone())),
                        ),
                )
                .when_some(nick, |d, nick| {
                    d.child(
                        div()
                            .type_style(text::CAPTION)
                            .text_color(t.muted)
                            .child(nick),
                    )
                })
        });
        Some(
            div()
                .mx(space::X1)
                .mb(space::X2)
                .p(space::X1)
                .rounded(radius::MD)
                .bg(t.surface)
                .border_1()
                .border_color(t.line)
                .shadow(t.elevation(Elevation::Overlay))
                .flex()
                .flex_col()
                .gap(space::X0_5)
                .children(rows)
                .into_any_element(),
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
        let working = self.working_indicator(cx);

        let box_el = div()
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
            .children(self.render_suggestions(cx))
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
            );

        div()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::accept_suggestion_action))
            .on_action(cx.listener(|this, _: &ui::text_input::Up, _window, cx| {
                if this.suggestions_open() && this.move_suggestion_selection(-1, cx) {
                    cx.stop_propagation();
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &ui::text_input::Down, _window, cx| {
                if this.suggestions_open() && this.move_suggestion_selection(1, cx) {
                    cx.stop_propagation();
                } else {
                    cx.propagate();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if !this.suggestions_open() {
                    return;
                }
                // Don't steal Shift+Arrow (text selection) or other modified navigation.
                if event.keystroke.modifiers.shift
                    || event.keystroke.modifiers.control
                    || event.keystroke.modifiers.alt
                    || event.keystroke.modifiers.platform
                {
                    return;
                }
                let key = event.keystroke.key.as_str();
                match key {
                    "up" if this.move_suggestion_selection(-1, cx) => cx.stop_propagation(),
                    "down" if this.move_suggestion_selection(1, cx) => cx.stop_propagation(),
                    _ => {}
                }
            }))
            .w_full()
            .flex()
            .flex_col()
            .gap(space::X1_5)
            .when_some(working, |d, label| {
                let base_style = window.text_style();
                let base = t.muted;
                let bright = t.ink;
                d.child(
                    div()
                        .px(space::X2)
                        .py(space::X0_5)
                        .type_style(text::SMALL)
                        .child(
                            div().id("working-indicator-shimmer").with_animation(
                                "working-indicator-shimmer",
                                Animation::new(Duration::from_millis(1800))
                                    .repeat()
                                    .with_easing(linear),
                                move |el, delta| {
                                    let text = working_indicator_text(
                                        label.as_ref(),
                                        delta,
                                        &base_style,
                                        base,
                                        bright,
                                    );
                                    el.child(text)
                                },
                            ),
                        ),
                )
            })
            .child(box_el)
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

    #[test]
    fn drafts_switch_and_restore_per_channel() {
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut drafts = HashMap::new();
        let a = Id(1);
        let b = Id(2);
        let draft_a = Draft {
            text: SharedString::from("hello"),
            selection: 2..5,
            reversed: false,
            reply: None,
        };

        // Save channel A, switch away, restore it.
        if draft_a.is_empty() {
            drafts.remove(&a);
        } else {
            drafts.insert(a, draft_a.clone());
        }
        let restored_a = drafts.get(&a).cloned().unwrap_or_else(Draft::empty);
        assert_eq!(restored_a, draft_a);

        // Empty draft is not kept.
        let empty = Draft::empty();
        if empty.is_empty() {
            drafts.remove(&b);
        } else {
            drafts.insert(b, empty.clone());
        }
        assert!(!drafts.contains_key(&b));
        assert_eq!(drafts.get(&b).cloned().unwrap_or_else(Draft::empty), empty);

        // Reply makes an otherwise empty text draft worth keeping.
        let reply = ReplyRef {
            message: Id(99),
            author: model::Author::Human,
            quote: Arc::from("quoted"),
        };
        let draft_reply = Draft {
            text: SharedString::default(),
            selection: 0..0,
            reversed: false,
            reply: Some(reply),
        };
        assert!(!draft_reply.is_empty());
        drafts.insert(b, draft_reply.clone());
        assert_eq!(
            drafts.get(&b).cloned().unwrap_or_else(Draft::empty),
            draft_reply
        );
    }

    #[test]
    fn core_echo_match_trims_and_scans_appended_range() {
        use std::sync::Arc;

        let mut log = model::ChannelLog::default();
        let human = |id: u32, body: &str| model::Message {
            id: Id(id),
            author: model::Author::Human,
            sent_at: 0,
            body: Arc::from(body),
            reply_to: None,
            stance: None,
        };
        let agent = |id: u32, body: &str| model::Message {
            id: Id(id),
            author: model::Author::Agent(Id(7)),
            sent_at: 0,
            body: Arc::from(body),
            reply_to: None,
            stance: None,
        };

        // Core stores trimmed body; draft may have trailing whitespace.
        let r1 = log.extend([human(1, "hello")]);
        assert!(echo_matches_in(Some(&log), r1.clone(), "hello  \n"));

        // If other messages land in the same appended range, still find the echo.
        let r2 = log.extend([agent(2, "reply"), human(3, "second")]);
        assert!(echo_matches_in(Some(&log), r2, "second\n"));

        // Different body does not match.
        assert!(!echo_matches_in(Some(&log), 0..log.len(), "other"));
    }

    #[test]
    fn mention_query_parses_at_token() {
        assert_eq!(
            mention_query("hello @cod", 10),
            Some((6..10, "cod".to_string()))
        );
        assert_eq!(mention_query("@al", 3), Some((0..3, "al".to_string())));
        assert_eq!(mention_query("email test@example.com", 20), None);
        // Cursor moved into the token: query is the prefix, range covers the whole mention token.
        assert_eq!(
            mention_query("hi @foo bar", 5),
            Some((3..7, "f".to_string()))
        );
        assert_eq!(mention_query("hello", 5), None);
        assert_eq!(mention_query("( @", 3), Some((2..3, String::new())));
        // Insertion/edit halfway through an existing mention keeps replacement over the full token.
        assert_eq!(
            mention_query("hi @claude there", 6),
            Some((3..10, "cl".to_string()))
        );
    }

    #[test]
    fn mention_filter_matches_handle_name_or_nickname() {
        let candidates = vec![
            MentionCandidate {
                handle: "claude".into(),
                name: "Claude Code".into(),
                nicknames: vec!["cx".into(), "cc".into()],
            },
            MentionCandidate {
                handle: "codex".into(),
                name: "Codex CLI".into(),
                nicknames: vec![],
            },
        ];
        assert_eq!(filter_mentions(&candidates, "cla").len(), 1);
        assert_eq!(filter_mentions(&candidates, "cli").len(), 1);
        assert_eq!(filter_mentions(&candidates, "CX").len(), 1);
        assert_eq!(filter_mentions(&candidates, "").len(), 2);
        assert!(filter_mentions(&candidates, "zzz").is_empty());
    }

    #[test]
    fn slash_command_query_detects_leading_command_context() {
        assert_eq!(
            slash_command_query("/di", 3),
            Some((0..3, "di".to_string(), String::new()))
        );
        assert_eq!(
            slash_command_query("/diff", 5),
            Some((0..5, "diff".to_string(), String::new()))
        );
        assert_eq!(
            slash_command_query("/diff --staged", 3),
            Some((0..5, "di".to_string(), " --staged".to_string()))
        );
        // Cursor in arguments: no command-name suggestions.
        assert_eq!(slash_command_query("/diff --staged", 6), None);
        assert_eq!(slash_command_query("see /diff", 9), None);
        assert_eq!(slash_command_query("not a command", 3), None);
        assert_eq!(
            slash_command_query("/ diff", 1),
            Some((0..1, String::new(), " diff".to_string()))
        );
    }

    #[test]
    fn slash_command_filter_matches_name_case_insensitively() {
        let candidates = vec![
            SlashCommandCandidate {
                name: "diff".into(),
                description: "Open diff".into(),
                argument_hint: None,
            },
            SlashCommandCandidate {
                name: "settings".into(),
                description: "Open settings".into(),
                argument_hint: Some("[section]".into()),
            },
        ];
        assert_eq!(filter_slash_commands(&candidates, "di").len(), 1);
        assert_eq!(filter_slash_commands(&candidates, "ETT").len(), 1);
        assert_eq!(filter_slash_commands(&candidates, "").len(), 2);
        assert!(filter_slash_commands(&candidates, "zzz").is_empty());
    }
}
