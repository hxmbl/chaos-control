//! The selected channel's messages.
//!
//! History is shown through GPUI's `list`, which only lays out and paints the rows
//! in view (plus a little overdraw) and remembers measured row heights, so a
//! channel with 100k messages costs about the same per frame as one with 20.
//! Messages arrive whole; appending splices rows onto the end of the list state
//! without touching the heights already measured.

use gpui::{
    AnyElement, Context, Entity, ListAlignment, ListOffset, ListState, SharedString, Subscription,
    Window, div, list, prelude::*, px,
};
use model::{Author, Id, PaneContent, pane};
use ui::{
    ActiveTheme, Ellipsis, IconName, Tooltip, TypeStyleExt, brand_icon, icon,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::{
    pane_host::PaneHost,
    store::{ChatStore, Reader, StoreEvent},
};

/// The empty state's blurb wraps at about 50 characters.
const EMPTY_MAX_W: f32 = 384.;

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
            StoreEvent::Receipts { channel, rows } if *channel == this.channel => {
                // Re-measure just these rows: read marks add or drop a line.
                let count = this.list.item_count();
                for &ix in rows {
                    if ix < count {
                        this.list.splice(ix..ix + 1, 1);
                    }
                }
                cx.notify();
            }
            StoreEvent::Receipts { .. } => {}
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
                    icon(brand_icon(agent.brand), icons::MD, t.ink).into_any_element(),
                    agent.name.clone().into(),
                ),
                None => (div().into_any_element(), "Agent".into()),
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

        div()
            .w_full()
            .flex()
            .justify_center()
            .child(
                div()
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
                                    // A refusal or dissent is a normal peer
                                    // reply: one muted word, no colour (colour
                                    // means status), never an error style.
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
                            })
                            .map(|d| {
                                let readers = store.readers(self.channel, ix);
                                if readers.is_empty() {
                                    d
                                } else {
                                    d.child(read_marks(ix, &readers, store.workspace(), &t))
                                }
                            }),
                    ),
            )
            .into_any_element()
    }
}

/// Small avatars of who has read up to this message and no further, at the
/// end of the row (Slack/iMessage style); hover lists names and times. Read
/// with no reply is a normal outcome, so the marks stay quiet.
fn read_marks(ix: usize, readers: &[Reader], ws: &model::Workspace, t: &ui::Theme) -> AnyElement {
    let names = readers
        .iter()
        .map(|r| format!("{} at {}", r.name, clock(r.read_at)))
        .collect::<Vec<_>>()
        .join(", ");
    let tip = format!("Read by {names}");
    let marks = readers.iter().map(|r| {
        let face: AnyElement = match r.author {
            Author::Agent(id) => match ws.agent(id) {
                Some(agent) => icon(brand_icon(agent.brand), icons::SM, t.muted).into_any_element(),
                None => initial(&r.name, t),
            },
            _ => initial(&r.name, t),
        };
        div()
            .size(size::READ_MARK)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(t.sunken)
            .child(face)
    });
    div()
        .id(("read-marks", ix))
        .mt(space::X1)
        .flex()
        .justify_end()
        .gap(space::X0_5)
        .children(marks)
        .tooltip(Tooltip::text(tip))
        .into_any_element()
}

/// A reader without a brand mark: their first letter.
fn initial(name: &str, t: &ui::Theme) -> AnyElement {
    let c: String = name.chars().take(1).collect();
    div()
        .type_style(text::CAPTION)
        .font_weight(text::SEMIBOLD)
        .text_color(t.muted)
        .child(SharedString::from(c))
        .into_any_element()
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
}

/// "HH:MM" (UTC) without pulling in a date library. Good enough for sample data.
fn clock(secs: u64) -> SharedString {
    let minutes = secs / 60;
    format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60).into()
}

/// The channel header, Discord style:
///
/// ```text
/// # main
/// ~/Projects/everything ::1
/// ```
///
/// `::N` is the channel's number within its project (creation order, never
/// reused, unchanged by renames). Channels that only exist locally have no
/// number and show the path alone.
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

impl Render for Conversation {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let store = self.store.read(cx);
        let channel = store.selected_channel();
        let header = header(channel, &store.workspace().project, &t);
        let body = self.render_body(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Conversation {
    fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
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
                        // flex_none: the blurb below is measured unwrapped first,
                        // so the column can come out short and squash its items.
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
