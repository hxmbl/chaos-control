//! Titlebar search: the field, the panel it grows into and the jump to a hit.
//!
//! At rest it is the pill in the titlebar. Focusing it (a click or cmd/ctrl-K)
//! grows that same pill into the big search panel: one field on top, the
//! highlighted result as a card with its actions, then the other results.
//! Closing shrinks it back. The workspace draws the open panel over
//! everything; the titlebar draws the pill only while it is closed, so the
//! one `TextInput` is never on screen twice. Opening is explicit (a click on
//! the pill, cmd/ctrl-K, typing into the field); focus leaving the field for
//! something else in the window closes it.
//!
//! Every keystroke snapshots the store (`ChatStore::search_corpus`, `Arc` clones
//! only) and runs the `SearchProvider` on a background thread, so a 100k-message
//! history never stalls typing. A newer query cancels the one in flight. The
//! provider is a plain substring scan for now; swapping it changes nothing here.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Pixels, SharedString,
    Subscription, Task, Window, actions, div, prelude::*, px,
};
use model::{
    Author, PaneActionKind, PaneContent, SearchAction, SearchHit, SearchProvider, SearchResult,
};
use ui::{
    ActiveTheme, Ellipsis, IconName, Pane, QuickAction, TextInput, TextInputEvent, Tooltip, icon,
    pill, truncated,
};

use crate::{settings, store::ChatStore};

actions!(search, [FocusSearch, SelectNext, SelectPrevious, Dismiss]);

/// Key context of the search field (up/down/escape bindings).
pub const KEY_CONTEXT: &str = "Search";
/// At most this many results.
const LIMIT: usize = 20;
/// Rows shown under the card before the "…" row hides the rest.
const COLLAPSED_ROWS: usize = 4;
/// The pill grows into the panel over this long.
const GROW: Duration = Duration::from_millis(140);
/// The titlebar pill: 396px wide, shrinking to 46% of narrow windows.
pub const PILL_HEIGHT: Pixels = px(30.);
pub fn pill_width(viewport: Pixels) -> Pixels {
    (viewport * 0.46).min(px(396.))
}
/// The open panel: half the window, between 560 and 760px, never closer than
/// 24px to either edge.
fn panel_width(viewport: Pixels) -> Pixels {
    (viewport * 0.5)
        .max(px(560.))
        .min(px(760.))
        .min(viewport - px(48.))
}

pub enum SearchEvent {
    /// The user picked a result; the store has already jumped to it.
    Jumped,
    /// Escape on an empty field or a click outside closed the panel; the
    /// keyboard should go back to the composer.
    Dismissed,
    /// The user picked one of a result's actions. Nothing produces actions
    /// yet (see `SearchAction`); the workspace will route these.
    #[allow(dead_code)]
    Action {
        hit: SearchHit,
        action: SearchAction,
    },
}

pub struct Search {
    store: Entity<ChatStore>,
    input: Entity<TextInput>,
    provider: Arc<dyn SearchProvider>,
    hits: Vec<SearchResult>,
    highlighted: usize,
    /// Whether the "…" row was opened to show every result.
    expanded: bool,
    /// The panel is open (the field has focus).
    open: bool,
    /// When it last opened, for the grow animation.
    opened_at: Option<Instant>,
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
        let focus = input.focus_handle(cx);
        let subscriptions = vec![
            // Focus moving elsewhere in the app closes the panel. Not when the
            // whole window deactivates (switching apps), so it's still there
            // when the user comes back.
            cx.on_focus_out(&focus, window, |this, _, window, cx| {
                if window.is_window_active() {
                    this.close(cx);
                }
            }),
            cx.subscribe_in(
                &input,
                window,
                |this, input, event, window, cx| match event {
                    TextInputEvent::Changed => {
                        // Typing into the field always shows the panel.
                        if !this.open && input.focus_handle(cx).is_focused(window) {
                            this.open(cx);
                        } else {
                            this.query_changed(cx);
                        }
                    }
                    TextInputEvent::Submit => this.confirm(this.highlighted, window, cx),
                },
            ),
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
            opened_at: None,
            searching: false,
            cancel: Arc::new(AtomicBool::new(false)),
            _pending: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The search-in-pane experiment (Settings, off by default): results open
    /// in the glass pane instead of the dropdown.
    pub fn in_pane(cx: &App) -> bool {
        settings::get(cx).search_in_pane
    }

    /// The current query, for the titlebar's stand-in pill while the field is
    /// in the pane.
    pub fn query(&self, cx: &App) -> SharedString {
        self.input.read(cx).text().clone()
    }

    /// cmd/ctrl-K: focus the field and open the panel. Pressed again with the
    /// panel open, it selects the query so typing replaces it.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.input.update(cx, |input, cx| input.select_all_text(cx));
        } else {
            window.focus(&self.input.focus_handle(cx));
            self.open(cx);
        }
    }

    /// Grow the pill into the panel (a click on the pill, cmd/ctrl-K, typing).
    pub fn open(&mut self, cx: &mut Context<Self>) {
        if self.open {
            return;
        }
        self.open = true;
        self.opened_at = Some(Instant::now());
        self.input
            .update(cx, |input, cx| input.set_centered(false, cx));
        self.query_changed(cx);
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            self.opened_at = None;
            self.cancel.store(true, Ordering::Relaxed);
            self._pending = None;
            self.input
                .update(cx, |input, cx| input.set_centered(true, cx));
            cx.notify();
        }
    }

    /// Close and hand the keyboard back (Escape, a click outside).
    fn dismiss_panel(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.close(cx);
            cx.emit(SearchEvent::Dismissed);
        }
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        let query = self.input.read(cx).text().to_string();
        if query.trim().is_empty() {
            self.hits.clear();
            self.searching = false;
            self._pending = None;
            cx.notify();
            return;
        }

        if !self.open {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
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

    /// Escape clears the query; on an empty field it closes the panel.
    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.input.read(cx).text().is_empty() {
            self.dismiss_panel(cx);
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
    /// The titlebar shows it in the pill while the panel is closed.
    pub fn field(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let this = cx.entity();
        div()
            .w_full()
            .key_context(KEY_CONTEXT)
            .on_action(window.listener_for(&this, Self::select_next))
            .on_action(window.listener_for(&this, Self::select_previous))
            .on_action(window.listener_for(&this, Self::dismiss))
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
                    Author::System => "Chaos Control".into(),
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

impl Search {
    /// A result's actions ("New chat", "Ask an agent to the side", …) as rows
    /// under the card, as in the search mock. Empty until a provider makes them.
    fn render_actions(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Some(result) = self.hits.get(self.highlighted) else {
            return Vec::new();
        };
        result
            .actions
            .iter()
            .enumerate()
            .map(|(ix, action)| {
                let hit = result.hit.clone();
                let action = action.clone();
                let label = SharedString::from(action.label.clone());
                pill(("search-action", ix), cx)
                    .h(px(38.))
                    .px(px(14.))
                    .text_size(px(13.5))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close(cx);
                        cx.emit(SearchEvent::Action {
                            hit: hit.clone(),
                            action: action.clone(),
                        });
                    }))
                    .child(div().flex_1().ellipsis().child(truncated(label)))
                    .into_any_element()
            })
            .collect()
    }
}

impl Search {
    /// Search in the pane (the experiment): the field takes the title's place
    /// beside the close dot, results fill the body, and the highlighted
    /// result's actions are the quick actions, led by "Go to …". Same
    /// provider, keys and jump as the dropdown. `None` unless search is open
    /// in pane mode.
    pub fn render_pane(
        &mut self,
        max_width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.open || !Self::in_pane(cx) {
            return None;
        }
        let t = *cx.theme();
        let progress = self.opened_at.map_or(1., |at| {
            (at.elapsed().as_secs_f32() / GROW.as_secs_f32()).min(1.)
        });
        if progress < 1. {
            window.request_animation_frame();
        }
        let eased = 1. - (1. - progress).powi(3);

        let query = self.input.read(cx).text().clone();
        let content = PaneContent::Search {
            query: Arc::from(query.as_ref()),
            results: self.hits.clone(),
            highlighted: self.highlighted,
        };
        let this = cx.entity();
        let actions: Vec<QuickAction> = content
            .actions()
            .into_iter()
            .enumerate()
            .map(|(ix, action)| {
                let this = this.clone();
                let hit = self.hits.get(self.highlighted).map(|r| r.hit.clone());
                let highlighted = self.highlighted;
                QuickAction::new(("search-pane-action", ix), action.label.clone(), {
                    move |window, cx| {
                        let kind = action.kind.clone();
                        this.update(cx, |this, cx| match kind {
                            PaneActionKind::Custom(id) => {
                                let label = action.label.clone();
                                this.close(cx);
                                if let Some(hit) = hit.clone() {
                                    cx.emit(SearchEvent::Action {
                                        hit,
                                        action: SearchAction { id, label },
                                    });
                                }
                            }
                            _ => this.confirm(highlighted, window, cx),
                        })
                    }
                })
            })
            .collect();

        let empty = self.hits.is_empty();
        let rows: Vec<gpui::AnyElement> = (0..self.hits.len())
            .filter(|ix| *ix != self.highlighted)
            .map(|ix| self.render_row(ix, cx))
            .collect();
        let field = self.field(window, cx);
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(30.))
            .text_size(px(18.))
            .line_height(px(24.))
            .child(icon(IconName::MagnifyingGlassBold, px(17.), t.muted))
            .child(div().flex_1().min_w_0().child(field));
        let close = cx.entity();
        let pane = Pane::new("search-pane")
            .anchor_top()
            .progress(eased)
            .max_width(max_width)
            .header(header)
            .on_close(move |_, cx| close.update(cx, |this, cx| this.dismiss_panel(cx)))
            .actions(actions)
            .map(|pane| {
                if query.trim().is_empty() {
                    pane.child(
                        div()
                            .text_size(px(13.5))
                            .text_color(t.muted)
                            .child("Search channels and messages"),
                    )
                } else if empty {
                    pane.child(div().text_size(px(13.5)).text_color(t.muted).child(
                        if self.searching {
                            "Searching…"
                        } else {
                            "No matches"
                        },
                    ))
                } else {
                    pane.child(div().ml(px(-6.)).child(self.render_card(cx)))
                        .children(rows)
                }
            });
        Some(pane.into_any_element())
    }
}

impl Render for Search {
    /// The open panel, shaped after the search mock: the titlebar pill grown
    /// into one rounded panel with the field on top, then the highlighted
    /// result as a card with its actions, a divider, the other results as
    /// stacked rows and a slim "…" row when there are more. Rendered by the
    /// workspace over everything else, centred where the pill is.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // In pane mode the workspace draws `render_pane` in the pane layer.
        if !self.open || Self::in_pane(cx) {
            return div().into_any_element();
        }
        let t = *cx.theme();
        let viewport = window.viewport_size().width;

        // Grow from the pill's width to the panel's, easing out.
        let grow = self.opened_at.map_or(1., |at| {
            (at.elapsed().as_secs_f32() / GROW.as_secs_f32()).min(1.)
        });
        if grow < 1. {
            window.request_animation_frame();
        }
        let eased = 1. - (1. - grow).powi(3);
        let (from, to) = (pill_width(viewport), panel_width(viewport));
        let width = from + (to - from) * eased;

        let query_empty = self.input.read(cx).text().trim().is_empty();
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
        let actions = self.render_actions(cx);
        let empty = self.hits.is_empty();
        let more = !self.expanded && hidden > 0;

        let field = self.field(window, cx);
        let body = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .opacity(eased)
            .when(query_empty, |d| {
                d.child(
                    div()
                        .px(px(12.))
                        .pt(px(2.))
                        .pb(px(8.))
                        .text_size(px(13.))
                        .text_color(t.muted)
                        .child("Search channels and messages"),
                )
            })
            .when(!query_empty && empty, |d| {
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
            .when(!actions.is_empty(), |d| d.children(actions))
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
            });

        div()
            .id("search-panel")
            .occlude()
            .w(width)
            .max_h(px(560.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(4.) * eased)
            .rounded(px(15.) + px(5.) * eased)
            .border_1()
            .border_color(t.line_strong)
            .bg(t.surface)
            .shadow(t.float_shadow())
            .text_color(t.ink)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_panel(cx)))
            // The field, where the pill was: icon on the left, query after it.
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .h(PILL_HEIGHT - px(2.) + px(6.) * eased)
                    .gap(px(8.))
                    .pl(px(11.))
                    .pr(px(12.))
                    .text_size(px(13.) + px(1.) * eased)
                    .line_height(px(18.))
                    .child(icon(IconName::MagnifyingGlassBold, px(15.), t.muted))
                    .child(div().flex_1().min_w_0().child(field)),
            )
            .child(body)
            .into_any_element()
    }
}
