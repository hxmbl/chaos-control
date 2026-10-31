//! The model picker: how a member is added. Opened from Agent Status's
//! "Add a model" row, drawn in the pane layer like search-in-pane.
//!
//! Members are models, so the picker lists models, not harnesses: one row
//! per model (`chaos_core::harness::Catalog` groups the same model offered
//! by several harnesses into one row). Choosing a row offered by one
//! harness adds it; a row several installed harnesses can run opens a
//! second step listing them, and nothing is added until the user picks one
//! (Up/Down and Enter, or a click; Escape goes back). There is no silent
//! default harness.
//!
//! Roster members whose harness isn't installed are listed at the bottom,
//! greyed, as "not installed · <harness>": they come back by themselves once
//! the harness is installed.
//!
//! The structure follows T3 Code's model picker (`ModelPickerContent.tsx`,
//! `ModelPickerSidebar.tsx`, MIT, github.com/pingdotgg/t3code): a search
//! field on top, a harness filter ("All" plus one entry per harness, which
//! T3 draws as a side rail and this narrower pane draws as a row), then the
//! ranked list. The ranking is ported from its `modelPickerSearch.ts` (see
//! `harness::rank`).
//!
//! The picker never starts a harness. It shows the model lists the store
//! already holds: each harness's cached list (`chaos_core::model_cache`),
//! probed in the background when core starts. Opening it only asks the store
//! to refresh stale lists in the background (`ChatStore::refresh_models`);
//! rows update as answers land, and a harness whose probe failed (signed out,
//! say) says so in its filter entry.

use std::{sync::Arc, time::Instant};

use chaos_core::harness::{CatalogModel, ModelSource, Offer, rank};
use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, SharedString, Subscription,
    Window, actions, div, prelude::*,
};
use ui::{
    ActiveTheme, Ellipsis, IconName, Pane, TextInput, TextInputEvent, Tooltip, TypeStyleExt,
    brand_icon, icon, pill,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::{agents::parse_brand, store::ChatStore};

actions!(model_picker, [SelectNext, SelectPrevious, Dismiss]);

/// Key context for the picker's field (see the bindings in `main.rs`).
pub const KEY_CONTEXT: &str = "ModelPicker";

/// How long the pane takes to grow in.
const GROW: std::time::Duration = std::time::Duration::from_millis(140);

pub enum ModelPickerEvent {
    /// Closed; the workspace hands the keyboard back.
    Dismissed,
}

pub struct ModelPicker {
    store: Entity<ChatStore>,
    input: Entity<TextInput>,
    open: bool,
    opened_at: Option<Instant>,
    /// Harness filter: `None` is "All".
    filter: Option<&'static str>,
    /// Keyboard highlight in the ranked rows.
    highlighted: usize,
    /// The row waiting for its harness to be chosen (by catalog key).
    choosing: Option<String>,
    /// Keyboard highlight among that row's harnesses.
    choice: usize,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ModelPickerEvent> for ModelPicker {}

impl Focusable for ModelPicker {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl ModelPicker {
    pub fn new(store: Entity<ChatStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Search models", cx));
        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                TextInputEvent::Changed => {
                    this.highlighted = 0;
                    this.choosing = None;
                    cx.notify();
                }
                TextInputEvent::Submit => this.submit(window, cx),
            }),
            // Model lists land in the store while the picker is open.
            cx.observe(&store, |this, _, cx| {
                if this.open {
                    cx.notify();
                }
            }),
        ];
        Self {
            store,
            input,
            open: false,
            opened_at: None,
            filter: None,
            highlighted: 0,
            choosing: None,
            choice: 0,
            _subscriptions: subscriptions,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens with an empty query, showing the lists the store already has.
    /// Stale lists are refreshed in the background; nothing here waits on a
    /// harness.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.opened_at = Some(Instant::now());
        self.filter = None;
        self.highlighted = 0;
        self.choosing = None;
        self.input.update(cx, |i, cx| i.clear(cx));
        window.focus(&self.input.focus_handle(cx));
        self.store.update(cx, |s, cx| s.refresh_models(cx));
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            self.opened_at = None;
            self.choosing = None;
            cx.emit(ModelPickerEvent::Dismissed);
            cx.notify();
        }
    }

    /// The rows for the current query and filter, best first.
    fn rows(&self, cx: &gpui::App) -> Vec<CatalogModel> {
        let catalog = self.store.read(cx).catalog();
        let query = self.input.read(cx).text().to_string();
        let pool: Vec<&CatalogModel> = match self.filter {
            Some(h) => catalog.for_harness(h).collect(),
            None => catalog.models.iter().collect(),
        };
        rank(pool, &query).into_iter().cloned().collect()
    }

    /// The harnesses row `row` can still be added through: under the current
    /// filter, and not already running that model as a member.
    fn choices(&self, row: &CatalogModel, cx: &gpui::App) -> Vec<Offer> {
        let store = self.store.read(cx);
        row.offers
            .iter()
            .filter(|o| self.filter.is_none_or(|f| f == o.harness))
            .filter(|o| !store.has_member(o.harness, o.model.as_deref()))
            .cloned()
            .collect()
    }

    /// Enter: add the highlighted harness when one is being chosen, else
    /// confirm the highlighted row.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = self.choosing.clone() {
            let rows = self.rows(cx);
            if let Some(row) = rows.iter().find(|r| r.key == key) {
                let choices = self.choices(row, cx);
                if let Some(o) = choices.get(self.choice.min(choices.len().saturating_sub(1))) {
                    let (harness, model, name) = (o.harness, o.model.clone(), row.name.clone());
                    self.add(harness, model, name, window, cx);
                }
                return;
            }
            // The row is gone (the query changed): start over from the list.
            self.choosing = None;
        }
        self.confirm(self.highlighted, window, cx);
    }

    /// Enter or a click on row `ix`: add it when only one installed harness
    /// can run it, else ask which one (required: nothing is added until the
    /// user picks).
    fn confirm(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let Some(row) = rows.get(ix) else {
            return;
        };
        let choices = self.choices(row, cx);
        match choices.as_slice() {
            [] => {}
            [only] => self.add(
                only.harness,
                only.model.clone(),
                row.name.clone(),
                window,
                cx,
            ),
            _ => {
                if self.choosing.as_deref() != Some(row.key.as_str()) {
                    self.choosing = Some(row.key.clone());
                    self.choice = 0;
                }
                cx.notify();
            }
        }
    }

    fn add(
        &mut self,
        harness: &str,
        model: Option<Arc<str>>,
        name: Arc<str>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store
            .update(cx, |s, cx| s.add_member(harness, model, name, cx));
        self.close(cx);
    }

    /// The number of harnesses the row being chosen offers, if one is.
    fn choosing_len(&self, cx: &gpui::App) -> Option<usize> {
        let key = self.choosing.as_deref()?;
        let rows = self.rows(cx);
        let row = rows.iter().find(|r| r.key == key)?;
        Some(self.choices(row, cx).len())
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = self.choosing_len(cx) {
            if n > 0 {
                self.choice = (self.choice + 1).min(n - 1);
                cx.notify();
            }
            return;
        }
        let n = self.rows(cx).len();
        if n > 0 {
            self.highlighted = (self.highlighted + 1).min(n - 1);
            cx.notify();
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if self.choosing_len(cx).is_some() {
            self.choice = self.choice.saturating_sub(1);
        } else {
            self.highlighted = self.highlighted.saturating_sub(1);
        }
        cx.notify();
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.choosing.take().is_some() {
            cx.notify();
        } else {
            self.close(cx);
        }
    }

    /// The pane, for the workspace's pane layer, while open.
    pub fn render_pane(
        &mut self,
        max_width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.open {
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

        let field = div()
            .w_full()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::dismiss))
            .child(self.input.clone());
        let header = div()
            .flex()
            .items_center()
            .gap(space::X2)
            .h(size::BUTTON)
            .type_style(text::TITLE)
            .font_weight(text::REGULAR)
            .child(icon(IconName::MagnifyingGlass, icons::MD, t.muted))
            .child(div().flex_1().min_w_0().child(field));

        let chips = self.store.read(cx).harness_chips();
        let rows = self.rows(cx);
        let highlighted = self.highlighted.min(rows.len().saturating_sub(1));

        // Harness filter: "All", then each harness with its mark. A harness
        // still answering, or whose list is only the preset guess, says so.
        let filter_row = div()
            .flex()
            .flex_wrap()
            .gap(space::X1)
            .child(self.filter_chip("all", None, "All", None, None, cx))
            .children(chips.iter().map(|c| {
                let tip = if c.checking {
                    Some(format!("Asking {} for its models…", c.name))
                } else if let Some(n) = &c.note {
                    Some(format!("{}: {n}. Showing its usual models.", c.name))
                } else if c.preset_only {
                    Some(format!(
                        "{}: its usual models (it hasn't listed its own yet)",
                        c.name
                    ))
                } else {
                    None
                };
                self.filter_chip(c.id, Some(c.id), c.name, Some(c.brand), tip, cx)
            }));

        let dormant = self.render_dormant(cx);
        let list = if chips.is_empty() {
            div()
                .type_style(text::SMALL)
                .text_color(t.muted)
                .child("No harnesses found. Install Codex, fx, Mistral Vibe or another one, then reopen Chaos Control.")
                .into_any_element()
        } else if rows.is_empty() {
            div()
                .type_style(text::SMALL)
                .text_color(t.muted)
                .child("No models match")
                .into_any_element()
        } else {
            div()
                .id("model-rows")
                .flex()
                .flex_col()
                .gap(space::X0_5)
                .max_h(gpui::px(360.))
                .overflow_y_scroll()
                .children(
                    rows.iter()
                        .enumerate()
                        .map(|(ix, row)| self.render_row(ix, row, ix == highlighted, cx)),
                )
                .into_any_element()
        };

        let close = cx.entity();
        let mut pane = Pane::new("model-picker")
            .anchor_top()
            .progress(eased)
            .max_width(max_width)
            .header(header)
            .on_close(move |_, cx| close.update(cx, |this, cx| this.close(cx)))
            .child(filter_row)
            .child(list);
        if let Some(d) = dormant {
            pane = pane.child(d);
        }
        Some(pane.into_any_element())
    }

    fn filter_chip(
        &self,
        key: &'static str,
        filter: Option<&'static str>,
        label: &'static str,
        brand: Option<model::Brand>,
        tip: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let t = *cx.theme();
        let selected = self.filter == filter;
        div()
            .id(SharedString::from(format!("harness-filter-{key}")))
            .flex()
            .items_center()
            .gap(space::X1)
            .h(size::CHIP)
            .px(space::X2)
            .rounded(radius::MD)
            .type_style(text::SMALL)
            .text_color(if selected { t.ink } else { t.ink_soft })
            .when(selected, |d| d.bg(t.selected))
            .hover(move |s| s.bg(t.hover))
            .when_some(brand, |d, b| {
                d.child(icon(brand_icon(b), icons::SM, t.ink_soft))
            })
            .child(label)
            .when_some(tip, |d, tip| d.tooltip(Tooltip::text(tip)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.filter = filter;
                this.highlighted = 0;
                this.choosing = None;
                cx.notify();
            }))
    }

    /// One model: its name, the harnesses that run it (marks), and "Added"
    /// when it is already here. A row waiting on a harness choice lists the
    /// installed harnesses that can run it under it, one highlighted.
    fn render_row(
        &self,
        ix: usize,
        row: &CatalogModel,
        highlighted: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let t = *cx.theme();
        let store = self.store.read(cx);
        let offers: Vec<_> = row
            .offers
            .iter()
            .filter(|o| self.filter.is_none_or(|f| f == o.harness))
            .collect();
        let all_added = offers
            .iter()
            .all(|o| store.has_member(o.harness, o.model.as_deref()));
        let guessed = offers.iter().all(|o| o.source == ModelSource::Preset);
        let via = offers
            .iter()
            .map(|o| o.harness_name)
            .collect::<Vec<_>>()
            .join(" · ");
        let choosing = self.choosing.as_deref() == Some(row.key.as_str());

        let main =
            pill(("model-row", ix), cx)
                .h(size::ROW)
                .gap(space::X2)
                .px(space::X2)
                .when(highlighted, |p| p.bg(t.selected))
                .child(div().flex().flex_none().gap(space::X0_5).children(
                    offers.iter().take(3).map(|o| {
                        icon(brand_icon(parse_brand(Some(o.icon))), icons::MD, t.ink_soft)
                    }),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .ellipsis()
                                .type_style(text::BODY)
                                .text_color(if all_added { t.muted } else { t.ink })
                                .child(truncated(SharedString::from(row.name.clone()))),
                        )
                        .child(
                            div()
                                .ellipsis()
                                .type_style(text::CAPTION)
                                .text_color(t.muted)
                                .child(truncated(SharedString::from(if guessed {
                                    format!("via {via} · usual list")
                                } else {
                                    format!("via {via}")
                                }))),
                        ),
                )
                .when(all_added, |p| {
                    p.child(
                        div()
                            .flex_none()
                            .type_style(text::CAPTION)
                            .text_color(t.muted)
                            .child("Added"),
                    )
                })
                .when(!all_added, |p| {
                    p.on_click(cx.listener(move |this, _, window, cx| {
                        this.highlighted = ix;
                        this.confirm(ix, window, cx);
                    }))
                });

        let mut col = div().flex().flex_col().child(main);
        if choosing {
            // Required step: one entry per installed harness that can run
            // this model; nothing is added until one is picked.
            let choices = self.choices(row, cx);
            let current = self.choice.min(choices.len().saturating_sub(1));
            let entries: Vec<_> = choices
                .iter()
                .enumerate()
                .map(|(n, o)| {
                    let (harness, model, name) = (o.harness, o.model.clone(), row.name.clone());
                    let on = n == current;
                    div()
                        .id(SharedString::from(format!("via-{ix}-{n}")))
                        .flex()
                        .items_center()
                        .gap(space::X2)
                        .h(size::BUTTON_SM)
                        .px(space::X2)
                        .rounded(radius::MD)
                        .type_style(text::SMALL)
                        .text_color(t.ink)
                        .when(on, |d| d.bg(t.selected))
                        .hover(move |s| s.bg(t.hover))
                        .child(icon(
                            brand_icon(parse_brand(Some(o.icon))),
                            icons::SM,
                            t.ink_soft,
                        ))
                        .child(div().flex_1().min_w_0().ellipsis().child(o.harness_name))
                        .when(o.source == ModelSource::Preset, |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .type_style(text::CAPTION)
                                    .text_color(t.muted)
                                    .child("usual list"),
                            )
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.add(harness, model.clone(), name.clone(), window, cx)
                        }))
                })
                .collect();
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::X0_5)
                    .pl(space::X8)
                    .py(space::X1)
                    .child(
                        div()
                            .type_style(text::CAPTION)
                            .text_color(t.muted)
                            .child("Choose a harness to run it: ↑ ↓ then Enter, Esc to go back"),
                    )
                    .children(entries),
            );
        }
        col.into_any_element()
    }

    /// Roster members whose harness isn't installed, greyed: "not installed
    /// · <harness>". Not clickable; they come back once the harness is
    /// installed. Hidden under a harness filter (only installed harnesses
    /// have one) and filtered by the search text.
    fn render_dormant(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.filter.is_some() {
            return None;
        }
        let t = *cx.theme();
        let query = self.input.read(cx).text().to_lowercase();
        let store = self.store.read(cx);
        let rows: Vec<_> = store
            .dormant()
            .iter()
            .filter(|d| {
                query.is_empty()
                    || d.name.to_lowercase().contains(&query)
                    || d.harness_name.to_lowercase().contains(&query)
            })
            .enumerate()
            .map(|(n, d)| {
                div()
                    .id(SharedString::from(format!("dormant-{n}")))
                    .flex()
                    .items_center()
                    .gap(space::X2)
                    .h(size::ROW)
                    .px(space::X2)
                    .tooltip(Tooltip::text(format!(
                        "{} isn't installed. Install it and this model comes back on the next start.",
                        d.harness_name
                    )))
                    .child(icon(brand_icon(d.brand), icons::MD, t.muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .ellipsis()
                                    .type_style(text::BODY)
                                    .text_color(t.muted)
                                    .child(truncated(SharedString::from(d.name.clone()))),
                            )
                            .child(
                                div()
                                    .ellipsis()
                                    .type_style(text::CAPTION)
                                    .text_color(t.muted)
                                    .child(truncated(SharedString::from(format!(
                                        "not installed · {}",
                                        d.harness_name
                                    )))),
                            ),
                    )
            })
            .collect();
        if rows.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap(space::X0_5)
                .child(
                    div()
                        .px(space::X2)
                        .type_style(text::CAPTION)
                        .text_color(t.muted)
                        .child("Not installed"),
                )
                .children(rows)
                .into_any_element(),
        )
    }
}
