//! Titlebar search: the field, the results dropdown and the jump to a hit.
//!
//! Every keystroke snapshots the store (`ChatStore::search_corpus`, `Arc` clones
//! only) and runs the `SearchProvider` on a background thread, so a 100k-message
//! history never stalls typing. A newer query cancels the one in flight. The
//! provider is a plain substring scan for now; swapping it changes nothing here.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, MouseButton,
    SharedString, Subscription, Task, Window, actions, div, prelude::*, px,
};
use model::{Author, SearchHit, SearchProvider, SearchResult};
use ui::{
    ActiveTheme, Ellipsis, IconName, TextInput, TextInputEvent, Tooltip, icon, pill, truncated,
};

use crate::store::ChatStore;

actions!(search, [FocusSearch, SelectNext, SelectPrevious, Dismiss]);

/// Key context of the search field (up/down/escape bindings).
pub const KEY_CONTEXT: &str = "Search";
/// At most this many results.
const LIMIT: usize = 20;
/// Rows shown under the card before the "…" row hides the rest.
const COLLAPSED_ROWS: usize = 4;

pub enum SearchEvent {
    /// The user picked a result; the store has already jumped to it.
    Jumped,
}

pub struct Search {
    store: Entity<ChatStore>,
    input: Entity<TextInput>,
    provider: Arc<dyn SearchProvider>,
    hits: Vec<SearchResult>,
    highlighted: usize,
    /// Whether the "…" row was opened to show every result.
    expanded: bool,
    open: bool,
    searching: bool,
    cancel: Arc<AtomicBool>,
    _pending: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SearchEvent> for Search {}

impl Search {
    pub fn new(
        store: Entity<ChatStore>,
        provider: Arc<dyn SearchProvider>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new("Find anything", cx).centered());
        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                TextInputEvent::Changed => this.query_changed(cx),
                TextInputEvent::Submit => this.confirm(this.highlighted, window, cx),
            }),
            // Results point into the store; drop them when the store changes shape.
            cx.observe(&store, |this, _, cx| {
                if this.open {
                    this.query_changed(cx);
                }
            }),
        ];
        Self {
            store,
            input,
            provider,
            hits: Vec::new(),
            highlighted: 0,
            expanded: false,
            open: false,
            searching: false,
            cancel: Arc::new(AtomicBool::new(false)),
            _pending: None,
            _subscriptions: subscriptions,
        }
    }

    /// cmd/ctrl-K: focus the field and show results for whatever is in it.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.input.focus_handle(cx));
        self.reopen(cx);
    }

    /// Show the dropdown again if there is a query to show results for.
    pub fn reopen(&mut self, cx: &mut Context<Self>) {
        if !self.open && !self.input.read(cx).text().trim().is_empty() {
            self.open = true;
            self.query_changed(cx);
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            self.cancel.store(true, Ordering::Relaxed);
            self._pending = None;
            cx.notify();
        }
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        let query = self.input.read(cx).text().to_string();
        if query.trim().is_empty() {
            self.hits.clear();
            self.open = false;
            self.searching = false;
            self._pending = None;
            cx.notify();
            return;
        }

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        self.open = true;
        self.searching = true;
        let corpus = self.store.read(cx).search_corpus();
        let provider = self.provider.clone();
        let scan =
            cx.background_spawn(async move { provider.search(&corpus, &query, LIMIT, &cancel) });
        // Replacing the task drops the previous one, so a stale result never lands.
        self._pending = Some(cx.spawn(async move |this, cx| {
            let hits = scan.await;
            this.update(cx, |this, cx| {
                this.hits = hits;
                this.highlighted = 0;
                this.expanded = false;
                this.searching = false;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.hits.is_empty() {
            self.highlighted = (self.highlighted + 1) % self.hits.len();
            // Walking past the collapsed rows opens the rest.
            if self.highlighted > COLLAPSED_ROWS {
                self.expanded = true;
            }
            cx.notify();
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if !self.hits.is_empty() {
            self.highlighted = self
                .highlighted
                .checked_sub(1)
                .unwrap_or(self.hits.len() - 1);
            if self.highlighted > COLLAPSED_ROWS {
                self.expanded = true;
            }
            cx.notify();
        }
    }

    /// Escape closes the dropdown; a second Escape clears the query.
    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.close(cx);
        } else {
            self.input.update(cx, |input, cx| input.clear(cx));
        }
    }

    fn confirm(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.hits.get(ix).map(|r| r.hit.clone()) else {
            return;
        };
        self.store.update(cx, |store, cx| match hit {
            SearchHit::Channel { channel } => store.select(channel, cx),
            SearchHit::Message { channel, index, .. } => store.reveal(channel, index, cx),
        });
        self.close(cx);
        cx.emit(SearchEvent::Jumped);
    }

    /// Wraps the field so up/down/escape reach the search while it has focus.
    pub fn field(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let this = cx.entity();
        div()
            .w_full()
            .key_context(KEY_CONTEXT)
            .on_action(window.listener_for(&this, Self::select_next))
            .on_action(window.listener_for(&this, Self::select_previous))
            .on_action(window.listener_for(&this, Self::dismiss))
            .on_mouse_down(
                MouseButton::Left,
                window.listener_for(&this, |this, _, _, cx| this.reopen(cx)),
            )
            .child(self.input.clone())
    }

    /// Title, trailing id and description for a hit, shared by card and rows.
    fn describe(&self, hit: &SearchHit, cx: &App) -> (SharedString, SharedString, SharedString) {
        let store = self.store.read(cx);
        let ws = store.workspace();
        match hit {
            SearchHit::Channel { channel } => {
                let c = ws.channel(*channel);
                let name = c.map_or("unknown".into(), |c| c.name.clone());
                let id = c
                    .and_then(|c| c.linear_issue.clone())
                    .map_or("Channel".into(), SharedString::from);
                let about = match c {
                    Some(c) if c.project_wide => format!(
                        "The project-wide room for {}. Everyone, human or AI, can talk here.",
                        ws.project.name
                    ),
                    Some(c) if c.linear_issue.is_some() => {
                        format!("Channel linked to {} on Linear.", c.name)
                    }
                    _ => "Channel.".to_string(),
                };
                (format!("#{name}").into(), id, about.into())
            }
            SearchHit::Message {
                channel,
                author,
                sent_at,
                snippet,
                ..
            } => {
                let channel = ws
                    .channel(*channel)
                    .map_or("unknown".into(), |c| c.name.clone());
                let who = match author {
                    Author::Human => "Hx".into(),
                    Author::Agent(id) => ws.agent(*id).map_or("Agent".into(), |a| a.name.clone()),
                };
                let minutes = sent_at / 60;
                let time = format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60);
                (
                    format!("{who} in #{channel}").into(),
                    time.into(),
                    SharedString::from(snippet.clone()),
                )
            }
        }
    }

    /// The highlighted result, as the card at the top of the panel.
    fn render_card(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = *cx.theme();
        let Some(result) = self.hits.get(self.highlighted) else {
            return div().into_any_element();
        };
        let (title, id, about) = self.describe(&result.hit, cx);
        let ix = self.highlighted;
        div()
            .id("search-card")
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(12.))
            .py(px(10.))
            .rounded(px(14.))
            .hover(move |s| s.bg(t.hover))
            .on_click(cx.listener(move |this, _, window, cx| this.confirm(ix, window, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(icon(
                        match result.hit {
                            SearchHit::Channel { .. } => IconName::Hash,
                            SearchHit::Message { .. } => IconName::MagnifyingGlassBold,
                        },
                        px(16.),
                        t.ink_soft,
                    ))
                    .child(
                        div()
                            .flex_shrink()
                            .ellipsis()
                            .text_size(px(15.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(truncated(title)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(13.))
                            .text_color(t.muted)
                            .child(id),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .line_clamp(3)
                    .text_size(px(13.5))
                    .line_height(px(20.))
                    .text_color(t.ink_soft)
                    .child(about),
            )
            .into_any_element()
    }

    /// Another result, as a full-width rounded row under the divider.
    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = *cx.theme();
        let (title, id, about) = self.describe(&self.hits[ix].hit, cx);
        let line: SharedString = match self.hits[ix].hit {
            SearchHit::Channel { .. } => title,
            SearchHit::Message { .. } => format!("{title}: {about}").into(),
        };
        pill(("search-row", ix), cx)
            .h(px(38.))
            .gap(px(10.))
            .px(px(14.))
            .text_size(px(13.5))
            .on_click(cx.listener(move |this, _, window, cx| this.confirm(ix, window, cx)))
            .child(div().flex_1().ellipsis().child(truncated(line)))
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .child(id),
            )
            .into_any_element()
    }
}

impl Focusable for Search {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for Search {
    /// The dropdown, shaped after the search sketch: one rounded panel hanging
    /// from the titlebar with the query line, the highlighted result as a card,
    /// a divider, then the other results as stacked rows and a slim "…" row when
    /// there are more. (In the sketch those rows are actions on the card; they
    /// arrive with the command palette and will use the same rows.) Rendered by
    /// the workspace over everything else, at the chat column's width.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let t = *cx.theme();
        let query = self.input.read(cx).text().clone();
        let others: Vec<usize> = (0..self.hits.len())
            .filter(|ix| *ix != self.highlighted)
            .collect();
        let hidden = others.len().saturating_sub(COLLAPSED_ROWS);
        let shown = if self.expanded || hidden == 0 {
            others.len()
        } else {
            COLLAPSED_ROWS
        };
        let rows: Vec<gpui::AnyElement> = others[..shown]
            .iter()
            .map(|ix| self.render_row(*ix, cx))
            .collect();
        let empty = self.hits.is_empty();
        let more = !self.expanded && hidden > 0;

        div()
            .id("search-results")
            .occlude()
            .w_full()
            .max_h(px(520.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(8.))
            .rounded(px(18.))
            .border_1()
            .border_color(t.line_strong)
            .bg(t.surface)
            .shadow(t.float_shadow())
            .text_color(t.ink)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            // Query line
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(6.))
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .child(icon(IconName::MagnifyingGlassBold, px(13.), t.muted))
                    .child(div().flex_1().ellipsis().child(truncated(query))),
            )
            .when(empty, |d| {
                d.child(
                    div()
                        .px(px(12.))
                        .py(px(10.))
                        .text_size(px(13.5))
                        .text_color(t.muted)
                        .child(if self.searching {
                            "Searching…"
                        } else {
                            "No matches"
                        }),
                )
            })
            .when(!empty, |d| d.child(self.render_card(cx)))
            .when(!rows.is_empty(), |d| {
                d.child(
                    div()
                        .mx(px(4.))
                        .h(px(3.))
                        .flex_none()
                        .rounded_full()
                        .bg(t.line),
                )
                .children(rows)
            })
            .when(more, |d| {
                d.child(
                    pill("search-more", cx)
                        .h(px(16.))
                        .justify_center()
                        .gap(px(6.))
                        .text_size(px(11.))
                        .line_height(px(12.))
                        .text_color(t.muted)
                        .tooltip(Tooltip::text(format!("{hidden} more")))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.expanded = true;
                            cx.notify();
                        }))
                        .child("…")
                        .child(icon(IconName::CaretDown, px(10.), t.muted)),
                )
            })
            .into_any_element()
    }
}
