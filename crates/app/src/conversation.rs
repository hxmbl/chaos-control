//! The selected channel's messages.
//!
//! History is shown through GPUI's `list`, which only lays out and paints the rows
//! in view (plus a little overdraw) and remembers measured row heights, so a
//! channel with 100k messages costs about the same per frame as one with 20.
//! Messages arrive whole; appending splices rows onto the end of the list state
//! without touching the heights already measured.

use std::{borrow::Cow, sync::Arc};

use gpui::{
    AnyElement, App, ClipboardItem, Context, Corner, ElementId, Entity, ListAlignment, ListOffset,
    ListState, MouseButton, MouseDownEvent, Pixels, Point, SharedString, StyledText, Subscription,
    TextRun, WeakEntity, Window, anchored, deferred, div, list, prelude::*, px,
};
use model::{Author, Id, Message, PaneContent, ReplyRef, mentions, pane};
use ui::{
    ActiveTheme, Elevation, Ellipsis, IconName, Tooltip, TypeStyleExt, brand_icon, icon,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::{
    composer::Composer,
    mention_highlight::{self, KnownMentions},
    pane_host::PaneHost,
    store::{ChatStore, ReactionChip, Reader, StoreEvent},
};

/// Emoji reactions offered in the message actions menu.
const QUICK_REACTIONS: [&str; 6] = ["👍", "✅", "👀", "😂", "❤️", "💀"];

/// The empty state's blurb wraps at about 50 characters.
const EMPTY_MAX_W: f32 = 384.;

#[derive(Clone)]
struct MessageMenu {
    message_id: Id,
    channel: Id,
    index: usize,
    body: Arc<str>,
    reply: ReplyRef,
    can_react: bool,
    position: Point<Pixels>,
}

pub struct Conversation {
    store: Entity<ChatStore>,
    pane: Entity<PaneHost>,
    composer: Entity<Composer>,
    channel: Id,
    list: ListState,
    /// Message picked in search, tinted until the channel changes.
    highlighted: Option<usize>,
    /// The message under the pointer (by id), so hover tracks message identity.
    hovered: Option<Id>,
    menu: Option<MessageMenu>,
    this: WeakEntity<Self>,
    _store_events: Subscription,
}

impl Conversation {
    pub fn new(
        store: Entity<ChatStore>,
        pane: Entity<PaneHost>,
        composer: Entity<Composer>,
        cx: &mut Context<Self>,
    ) -> Self {
        let channel = store.read(cx).selected();
        let count = store.read(cx).log(channel).map_or(0, |l| l.len());
        let subscription = cx.subscribe(&store, |this, store, event, cx| match event {
            StoreEvent::Selected => {
                this.channel = store.read(cx).selected();
                let count = store.read(cx).log(this.channel).map_or(0, |l| l.len());
                // Bottom-aligned, so a reset lands on the newest message.
                this.list.reset(count);
                this.highlighted = None;
                this.hovered = None;
                this.menu = None;
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
            StoreEvent::Receipts { channel, rows } | StoreEvent::Reactions { channel, rows }
                if *channel == this.channel =>
            {
                // Re-measure just these rows: read marks and reaction chips
                // add or drop a line.
                let count = this.list.item_count();
                for &ix in rows {
                    if ix < count {
                        this.list.splice(ix..ix + 1, 1);
                    }
                }
                cx.notify();
            }
            StoreEvent::Receipts { .. } | StoreEvent::Reactions { .. } => {}
            StoreEvent::SendFailed { .. } => {}
        });
        Self {
            store,
            pane,
            composer,
            channel,
            list: ListState::new(count, ListAlignment::Bottom, px(600.)),
            highlighted: None,
            hovered: None,
            menu: None,
            this: cx.entity().downgrade(),
            _store_events: subscription,
        }
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    fn open_menu(
        &mut self,
        message: &Message,
        index: usize,
        can_react: bool,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.menu = Some(MessageMenu {
            message_id: message.id,
            channel: self.channel,
            index,
            body: message.body.clone(),
            reply: ReplyRef::to(message),
            can_react,
            position,
        });
        cx.notify();
    }

    fn reply_to(&mut self, reply: ReplyRef, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.composer
            .update(cx, |c, cx| c.set_reply(reply, window, cx));
        cx.notify();
    }

    fn copy_body(&mut self, body: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(body.to_string()));
        self.menu = None;
        cx.notify();
    }

    fn toggle_reaction_for(
        &mut self,
        channel: Id,
        index: usize,
        emoji: &str,
        cx: &mut Context<Self>,
    ) {
        self.store
            .update(cx, |s, cx| s.toggle_reaction(channel, index, emoji, cx));
        self.menu = None;
        cx.notify();
    }

    fn render_message(&self, ix: usize, window: &mut Window, cx: &mut App) -> AnyElement {
        let t = *cx.theme();
        let (message, avatar, name, can_react, chips, readers, display, known, is_long) = {
            let store = self.store.read(cx);
            let Some(message) = store.log(self.channel).and_then(|l| l.get(ix)).cloned() else {
                return div().into_any_element();
            };
            let (avatar, name): (AnyElement, SharedString) = match message.author {
                Author::Agent(id) => match store.workspace().agent(id) {
                    Some(agent) => (
                        icon(brand_icon(agent.brand), icons::MD, t.ink).into_any_element(),
                        agent.name.clone().into(),
                    ),
                    None => (div().into_any_element(), model::PREVIOUS_AGENT.into()),
                },
                Author::Human => (
                    div()
                        .type_style(text::CAPTION)
                        .font_weight(text::SEMIBOLD)
                        .child("Hx")
                        .into_any_element(),
                    "Hx".into(),
                ),
                Author::System => (
                    div()
                        .type_style(text::CAPTION)
                        .text_color(t.ink_soft)
                        .child("·")
                        .into_any_element(),
                    "Chaos Control".into(),
                ),
            };

            let can_react = store.can_react(self.channel, ix);
            let chips = store.reactions(self.channel, ix);
            let readers = store.readers(self.channel, ix);
            let is_long = pane::is_long(&message.body);
            let display = shown_body(store, &message.body);
            let known = known_mentions(store);
            (
                message, avatar, name, can_react, chips, readers, display, known, is_long,
            )
        };
        let this = self.this.clone();
        let this_right = self.this.clone();
        let body: AnyElement = if is_long {
            self.render_collapsed_with(ix, &message, &display, &known, &t, window)
        } else {
            div()
                .w_full()
                .min_w_0()
                .whitespace_normal()
                .text_color(t.ink_soft)
                .child(highlighted_text(display, &known, t.ink_soft, &t, window))
                .into_any_element()
        };

        let message_id = message.id;
        let hovered = self.hovered == Some(message_id);

        div()
            .w_full()
            .flex()
            .justify_center()
            .child(
                div()
                    .id(ElementId::Name(format!("message-row-{ix}").into()))
                    .on_hover(move |over: &bool, _, cx| {
                        this.update(cx, |c, cx| {
                            let next = if *over {
                                Some(message_id)
                            } else if c.hovered == Some(message_id) {
                                None
                            } else {
                                c.hovered
                            };
                            if next != c.hovered {
                                c.hovered = next;
                                cx.notify();
                            }
                        })
                        .ok();
                    })
                    .on_mouse_down(MouseButton::Right, {
                        let message = message.clone();
                        move |event: &MouseDownEvent, _window, cx| {
                            cx.stop_propagation();
                            let pos = event.position;
                            let msg = message.clone();
                            this_right
                                .update(cx, |c, cx| c.open_menu(&msg, ix, can_react, pos, cx))
                                .ok();
                        }
                    })
                    .relative()
                    .w_full()
                    .flex()
                    .gap(space::X3)
                    .px(space::X2)
                    .py(space::X2)
                    .rounded(radius::MD)
                    .when(self.highlighted == Some(ix), |row| row.bg(t.selected))
                    .child(
                        div()
                            .size(size::BUTTON)
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(t.sunken)
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
                                    .gap(space::X2)
                                    .min_w_0()
                                    .child(
                                        div()
                                            .flex_shrink()
                                            .ellipsis()
                                            .type_style(text::SMALL)
                                            .font_weight(text::SEMIBOLD)
                                            .child(truncated(name)),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .type_style(text::CAPTION)
                                            .text_color(t.muted)
                                            .child(clock(message.sent_at)),
                                    )
                                    .when_some(message.stance, |d, stance| {
                                        d.child(
                                            div()
                                                .flex_none()
                                                .type_style(text::CAPTION)
                                                .font_weight(text::MEDIUM)
                                                .text_color(t.muted)
                                                .child(stance.label()),
                                        )
                                    }),
                            )
                            .when_some(message.reply_to.as_ref(), |d, reply| {
                                let store = self.store.read(cx);
                                d.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(space::X1_5)
                                        .min_w_0()
                                        .mt(space::X0_5)
                                        .pl(space::X2)
                                        .border_l_2()
                                        .border_color(t.line_strong)
                                        .type_style(text::SMALL)
                                        .text_color(t.muted)
                                        .child(div().flex_none().font_weight(text::MEDIUM).child(
                                            SharedString::from(pane::author_name(
                                                store.workspace(),
                                                reply.author,
                                            )),
                                        ))
                                        .child(
                                            div()
                                                .flex_1()
                                                .ellipsis()
                                                .child(truncated(reply.quote.clone())),
                                        ),
                                )
                            })
                            .child(body)
                            .when(!chips.is_empty(), |d| {
                                d.child(self.reaction_chips(ix, &chips, can_react, &t))
                            })
                            .when(!readers.is_empty(), |d| {
                                d.child(read_marks(
                                    ix,
                                    &readers,
                                    self.store.read(cx).workspace(),
                                    &t,
                                ))
                            }),
                    )
                    .when(hovered, |row| {
                        row.child(self.action_bar(ix, &message, can_react, &t, window, cx))
                    }),
            )
            .into_any_element()
    }

    fn action_bar(
        &self,
        ix: usize,
        message: &Message,
        can_react: bool,
        t: &ui::Theme,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let this_reply = self.this.clone();
        let this_copy = self.this.clone();
        let this_more = self.this.clone();
        let reply = ReplyRef::to(message);
        let body = message.body.to_string();
        let message = message.clone();
        let hover = t.hover;

        let btn = |id: ElementId, label: &'static str| {
            div()
                .id(id)
                .h(size::BUTTON_SM)
                .px(space::X2)
                .flex()
                .items_center()
                .justify_center()
                .rounded(radius::MD)
                .type_style(text::SMALL)
                .text_color(t.ink_soft)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(label)
        };

        div()
            .absolute()
            .bottom(space::X1)
            .right(space::X2)
            .flex()
            .items_center()
            .gap(space::X1)
            .p(space::X0_5)
            .rounded(radius::MD)
            .bg(t.surface)
            .shadow(t.elevation(Elevation::Float))
            .child(
                btn(
                    ElementId::Name(format!("reply-action-{ix}").into()),
                    "Reply",
                )
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    let reply = reply.clone();
                    this_reply
                        .update(cx, |c, cx| c.reply_to(reply, window, cx))
                        .ok();
                }),
            )
            .child(
                btn(ElementId::Name(format!("copy-action-{ix}").into()), "Copy").on_click(
                    move |_, _, cx| {
                        cx.stop_propagation();
                        this_copy.update(cx, |c, cx| c.copy_body(&body, cx)).ok();
                    },
                ),
            )
            .child(
                btn(ElementId::Name(format!("more-action-{ix}").into()), "…").on_mouse_down(
                    MouseButton::Left,
                    move |event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let pos = event.position;
                        let msg = message.clone();
                        this_more
                            .update(cx, |c, cx| c.open_menu(&msg, ix, can_react, pos, cx))
                            .ok();
                    },
                ),
            )
            .into_any_element()
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_ref()?.clone();
        let t = *cx.theme();
        let this = cx.entity().downgrade();

        let mut items = div()
            .id("message-menu")
            .occlude()
            .min_w(px(180.))
            .p(space::X1)
            .rounded(radius::MD)
            .bg(t.surface)
            .border_1()
            .border_color(t.line)
            .shadow(t.elevation(Elevation::Overlay))
            .flex()
            .flex_col()
            .gap(space::X0_5)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(menu_item(
                "menu-reply",
                "Reply",
                t,
                cx.listener(move |this, _, window, cx| {
                    this.reply_to(menu.reply.clone(), window, cx);
                }),
            ))
            .child(menu_item("menu-copy", "Copy text", t, {
                let body = menu.body.to_string();
                cx.listener(move |this, _, _, cx| {
                    this.copy_body(&body, cx);
                })
            }));

        if menu.can_react {
            items = items.child(div().h(px(1.)).mx(space::X1).bg(t.line)).child(
                div()
                    .px(space::X2)
                    .pt(space::X1)
                    .pb(space::X0_5)
                    .type_style(text::CAPTION)
                    .text_color(t.muted)
                    .child("React"),
            );
            let row = QUICK_REACTIONS.iter().enumerate().fold(
                div()
                    .flex()
                    .items_center()
                    .gap(space::X1)
                    .px(space::X1)
                    .pb(space::X1),
                |row, (i, emoji)| {
                    let this = this.clone();
                    let emoji_str = *emoji;
                    row.child(
                        div()
                            .id(("menu-react", i))
                            .size(size::BUTTON_SM)
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius::MD)
                            .cursor_pointer()
                            .hover(|s| s.bg(t.hover))
                            .on_click(move |_, _, cx| {
                                this.update(cx, |c, cx| {
                                    c.toggle_reaction_for(menu.channel, menu.index, emoji_str, cx)
                                })
                                .ok();
                            })
                            .child(SharedString::from(*emoji)),
                    )
                },
            );
            items = items.child(row);
        }

        Some(
            deferred(
                anchored()
                    .position(menu.position)
                    .anchor(Corner::TopLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(cx)))
                            .child(items),
                    ),
            )
            .with_priority(10)
            .into_any_element(),
        )
    }

    /// Small, quiet chips under a message: the emoji and how many, in order
    /// of first use. Hover lists who; clicking adds the local human's own
    /// (or takes it back if it is already theirs).
    fn reaction_chips(
        &self,
        ix: usize,
        chips: &[ReactionChip],
        can_react: bool,
        t: &ui::Theme,
    ) -> AnyElement {
        let channel = self.channel;
        let items = chips.iter().enumerate().map(|(n, chip)| {
            let tip = format!("{} reacted {}", chip.names.join(", "), chip.emoji);
            let store = self.store.clone();
            let emoji = chip.emoji.clone();
            let hover = t.hover;
            div()
                .id(ElementId::Name(format!("reaction-{ix}-{n}").into()))
                .h(size::CHIP)
                .flex()
                .flex_none()
                .items_center()
                .gap(space::X1)
                .px(space::X1_5)
                .rounded(radius::MD)
                .bg(if chip.mine { t.selected } else { t.sunken })
                .type_style(text::CAPTION)
                .font_weight(text::MEDIUM)
                .text_color(if chip.mine { t.ink } else { t.muted })
                .child(SharedString::from(chip.emoji.clone()))
                .child(SharedString::from(chip.count.to_string()))
                .tooltip(Tooltip::text(tip))
                .when(can_react, move |d| {
                    d.cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            store.update(cx, |s, cx| s.toggle_reaction(channel, ix, &emoji, cx));
                        })
                })
        });
        div()
            .mt(space::X1)
            .flex()
            .flex_wrap()
            .gap(space::X1)
            .children(items)
            .into_any_element()
    }

    /// A long message's first lines, ending in "…", with "Open" to read it in
    /// full in the pane (see `model::pane::is_long` for the threshold).
    fn render_collapsed_with(
        &self,
        ix: usize,
        message: &Message,
        display: &SharedString,
        known: &KnownMentions,
        t: &ui::Theme,
        window: &mut Window,
    ) -> AnyElement {
        let preview = pane::preview(display.as_ref());
        let lines = preview.lines().map(|line| {
            let line: SharedString = line.to_string().into();
            div()
                .w_full()
                .min_w_0()
                .whitespace_normal()
                .child(highlighted_text(line, known, t.ink_soft, t, window))
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
            .gap(space::X0_5)
            .text_color(t.ink_soft)
            .children(lines)
            .child(
                div()
                    .mt(space::X1_5)
                    .flex()
                    .items_center()
                    .gap(space::X2)
                    .child(
                        div()
                            .id(("open-message", ix))
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(space::X1_5)
                            .h(size::BUTTON_SM)
                            .px(space::X3)
                            .rounded(radius::MD)
                            .bg(t.sunken)
                            .type_style(text::SMALL)
                            .font_weight(text::MEDIUM)
                            .text_color(t.ink)
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .on_click(move |_, window, cx| {
                                pane.update(cx, |p, cx| p.open(content.clone(), window, cx));
                            })
                            .child("Open")
                            .child(icon(IconName::CaretRight, icons::SM, t.muted)),
                    )
                    .child(
                        div()
                            .type_style(text::CAPTION)
                            .text_color(t.muted)
                            .child(SharedString::from(format!("{words} words"))),
                    ),
            )
            .into_any_element()
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.theme();
        let store = self.store.read(cx);
        let channel = store.selected_channel();

        if self.list.item_count() > 0 {
            let this = cx.entity().downgrade();
            return list(self.list.clone(), move |ix, window, cx| {
                this.upgrade()
                    .map(|view| {
                        view.update(cx, |view, view_cx| view.render_message(ix, window, view_cx))
                    })
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

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .px(space::X6)
            .child(
                div()
                    .max_w(px(EMPTY_MAX_W))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(space::X2)
                    .text_center()
                    .child(
                        div()
                            .size(size::ROW_LG)
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius::MD)
                            .bg(t.sunken)
                            .child(icon(IconName::Hash, icons::LG, t.faint)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .type_style(text::BODY)
                            .font_weight(text::SEMIBOLD)
                            .text_color(t.ink_soft)
                            .child(SharedString::from(channel.name.clone())),
                    )
                    .child(
                        div()
                            .type_style(text::SMALL)
                            .text_color(t.muted)
                            .child(blurb),
                    ),
            )
            .into_any_element()
    }
}

fn menu_item(
    id: impl Into<ElementId>,
    label: &'static str,
    t: ui::Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id.into())
        .px(space::X2)
        .py(space::X1_5)
        .rounded(radius::SM)
        .type_style(text::SMALL)
        .text_color(t.ink)
        .cursor_pointer()
        .hover(|s| s.bg(t.hover))
        .on_click(on_click)
        .child(label)
}

impl Render for Conversation {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let store = self.store.read(cx);
        let channel = store.selected_channel();
        let header = header(channel, &store.workspace().project, &t);
        let body = self.render_body(cx);
        let menu = self.render_menu(cx);
        div().size_full().flex().flex_col().child(header).child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(body)
                .children(menu),
        )
    }
}

/// A message's text as the chat draws it.
fn shown_body(store: &ChatStore, body: &Arc<str>) -> SharedString {
    match mentions::rewrite_former_mentions(body, |token| {
        store.is_former_project_agent_mention(token)
    }) {
        Cow::Borrowed(_) => SharedString::from(body.clone()),
        Cow::Owned(shown) => SharedString::from(shown),
    }
}

/// Known mention targets for highlighting.
fn known_mentions(store: &ChatStore) -> KnownMentions {
    let mut known = KnownMentions::new();
    for member in store.mention_candidates() {
        known.insert_member(
            member.handle.as_ref(),
            member.handle.as_ref(),
            &member.nicknames,
        );
    }
    known.insert_broadcasts();
    known
}

/// Builds a `StyledText` element for `text`, highlighting known mentions.
fn highlighted_text(
    text: SharedString,
    known: &KnownMentions,
    base_color: gpui::Hsla,
    theme: &ui::Theme,
    window: &mut Window,
) -> StyledText {
    let spans = mention_highlight::mention_highlights(&text, known, theme.appearance);
    let base = window.text_style();
    if spans.is_empty() {
        let run = TextRun {
            len: text.len(),
            font: base.font(),
            color: base_color,
            background_color: base.background_color,
            underline: base.underline,
            strikethrough: base.strikethrough,
        };
        return StyledText::new(text).with_runs(vec![run]);
    }
    let runs = runs_for_highlights(&text, &base, base_color, &spans);
    StyledText::new(text).with_runs(runs)
}

fn runs_for_highlights(
    text: &str,
    base: &gpui::TextStyle,
    base_color: gpui::Hsla,
    highlights: &[(std::ops::Range<usize>, gpui::HighlightStyle)],
) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut ix = 0;
    for (range, highlight) in highlights {
        if ix < range.start {
            debug_assert!(text.is_char_boundary(range.start));
            runs.push(TextRun {
                len: range.start - ix,
                font: base.font(),
                color: base_color,
                background_color: base.background_color,
                underline: base.underline,
                strikethrough: base.strikethrough,
            });
        }
        debug_assert!(text.is_char_boundary(range.end));
        let mut style = base.clone();
        style.color = base_color;
        let style = style.highlight(*highlight);
        runs.push(style.to_run(range.len()));
        ix = range.end;
    }
    if ix < text.len() {
        runs.push(TextRun {
            len: text.len() - ix,
            font: base.font(),
            color: base_color,
            background_color: base.background_color,
            underline: base.underline,
            strikethrough: base.strikethrough,
        });
    }
    runs
}

/// "HH:MM" (UTC)
fn clock(secs: u64) -> SharedString {
    let minutes = secs / 60;
    format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60).into()
}

/// The channel header
fn header(
    channel: &model::Channel,
    project: &model::Project,
    t: &ui::Theme,
) -> impl IntoElement + use<> {
    let place: SharedString = match (&project.path, channel.ordinal) {
        (Some(path), Some(n)) => format!("{path} ::{n}").into(),
        (Some(path), None) => path.to_string().into(),
        (None, Some(n)) => format!("{} ::{n}", project.name).into(),
        (None, None) => project.name.to_string().into(),
    };
    div()
        .flex_none()
        .flex()
        .flex_col()
        .px(space::X1)
        .pt(space::X3)
        .pb(space::X2)
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::X1_5)
                .type_style(text::TITLE)
                .text_color(t.ink)
                .child(icon(IconName::Hash, icons::MD, t.ink))
                .child(
                    div()
                        .min_w_0()
                        .ellipsis()
                        .child(truncated(SharedString::from(channel.name.clone()))),
                ),
        )
        .child(
            div()
                .min_w_0()
                .ellipsis()
                .type_style(text::CAPTION)
                .text_color(t.muted)
                .child(truncated(place)),
        )
}

const READ_MARKS_SHOWN: usize = 4;

fn read_marks(ix: usize, readers: &[Reader], ws: &model::Workspace, t: &ui::Theme) -> AnyElement {
    let names = readers
        .iter()
        .map(|r| format!("{} at {}", r.name, clock(r.read_at)))
        .collect::<Vec<_>>()
        .join(", ");
    let tip = format!("Read by {names}");
    let shown = readers.len().min(READ_MARKS_SHOWN);
    let more = readers.len().saturating_sub(shown);
    let marks = readers.iter().take(shown).enumerate().map(|(n, r)| {
        let name = match r.author {
            Author::Agent(id) => ws.agent(id).map_or(&*r.name, |a| &a.name),
            _ => &r.name,
        };
        div()
            .size(size::READ_MARK)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(t.sunken)
            .border_2()
            .border_color(t.canvas)
            .when(n > 0, |d| d.ml(px(-6.)))
            .child(initial(name, t))
    });
    div()
        .id(("read-marks", ix))
        .mt(space::X1)
        .flex()
        .items_center()
        .justify_end()
        .children(marks)
        .when(more > 0, |d| {
            d.child(
                div()
                    .size(size::READ_MARK)
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .ml(px(-6.))
                    .rounded_full()
                    .bg(t.sunken)
                    .border_2()
                    .border_color(t.canvas)
                    .type_style(text::CAPTION)
                    .font_weight(text::MEDIUM)
                    .text_color(t.muted)
                    .child(format!("+{more}")),
            )
        })
        .tooltip(Tooltip::text(tip))
        .into_any_element()
}

fn initial(name: &str, t: &ui::Theme) -> AnyElement {
    let c: String = name.chars().take(1).collect();
    div()
        .type_style(text::CAPTION)
        .font_weight(text::SEMIBOLD)
        .text_color(t.ink_soft)
        .child(SharedString::from(c))
        .into_any_element()
}
