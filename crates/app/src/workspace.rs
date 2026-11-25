//! Root view: lays out the titlebar, both floating sidebars, the conversation and
//! the composer, and keeps the theme in step with the OS appearance.

use std::time::Instant;

use gpui::{
    AnyView, Context, CursorStyle, Decorations, DispatchPhase, Entity, FocusHandle, Focusable,
    MouseButton, MouseExitEvent, MouseMoveEvent, MouseUpEvent, Pixels, ResizeEdge, StyleRefinement,
    Subscription, Window, canvas, div, prelude::*, px,
};
use ui::{
    ActiveTheme, Elevation, FONT_FAMILY, IconName, TextInput, Theme, Tooltip, TypeStyleExt, icon,
    tokens::{icon as icons, radius, size, space, text},
};

use crate::{
    composer::Composer,
    conversation::Conversation,
    model_picker::{ModelPicker, ModelPickerEvent},
    pane_host::{PaneEvent, PaneHost, pane_frame},
    panels::{
        GUTTER, Panels, RESIZE_HANDLE, SIDE_INSET, Side, SideFrame, ToggleLeftSidebar,
        ToggleRightSidebar,
    },
    profile,
    right_sidebar::RightSidebar,
    search::{FocusSearch, PILL_HEIGHT, Search, SearchEvent},
    sidebar::Sidebar,
    store::ChatStore,
    titlebar::Titlebar,
};

/// The sidebars start just under the titlebar.
const SIDE_TOP: Pixels = space::X1;
/// Corner radius of both sidebar panels (their views draw the same).
const SIDE_RADIUS: Pixels = radius::XL;
/// The conversation and composer widen with the window between these widths.
const COLUMN_MIN: Pixels = px(720.);
const COLUMN_MAX: Pixels = px(960.);
/// The composer's height with one line and no reply chip: borders, padding,
/// one 22px line of text and the 32px button row (see `composer.rs`). Used
/// until the composer has been measured.
const COMPOSER_ONE_LINE: Pixels = px(82.);
/// Gap between the newest message and the top of the composer.
const COMPOSER_GAP: Pixels = px(18.);
/// Height changes smaller than this are layout jitter, not a resize.
const CLEARANCE_EPSILON: Pixels = px(0.5);

/// Height kept clear under the conversation (and the pane) for the floating
/// composer: the composer itself, the inset under it and a small gap. Follows
/// the composer as it grows with its text or shows a reply chip, so a tall
/// composer never covers the newest messages (STRM-574). A one-line composer
/// gives the 112px the clearance used to be fixed at.
fn composer_clearance(composer_height: Pixels) -> Pixels {
    composer_height.max(COMPOSER_ONE_LINE) + SIDE_INSET + COMPOSER_GAP
}

/// Whether a newly measured composer height should move the clearance.
fn composer_height_changed(old: Pixels, new: Pixels) -> bool {
    (new - old).abs() > CLEARANCE_EPSILON
}
pub const TITLEBAR_HEIGHT: Pixels = px(52.);

/// Where the centre column goes: left and right padding plus its maximum width.
///
/// `left`/`right` are the widths the sidebars take from each edge (zero when
/// hidden). When there's room, both sides reserve the larger of the two so the
/// conversation and composer stay centred under the titlebar search; when that
/// would squeeze the column, it centres in the space actually left over.
fn centre_column(viewport: Pixels, left: Pixels, right: Pixels) -> (Pixels, Pixels, Pixels) {
    let max_w = (viewport * 0.6).max(COLUMN_MIN).min(COLUMN_MAX);
    let (l, r) = (left + GUTTER, right + GUTTER);
    let side = l.max(r);
    let fits_centred = viewport - side - side >= max_w.min(viewport - l - r);
    if fits_centred {
        (side, side, max_w)
    } else {
        (l, r, max_w)
    }
}

pub struct Workspace {
    store: Entity<ChatStore>,
    panels: Entity<Panels>,
    pane: Entity<PaneHost>,
    search: Entity<Search>,
    titlebar: Entity<Titlebar>,
    sidebar: Entity<Sidebar>,
    right_sidebar: Entity<RightSidebar>,
    model_picker: Entity<ModelPicker>,
    conversation: Entity<Conversation>,
    composer: Entity<Composer>,
    /// The composer's height on the last paint, which sets the clearance
    /// under the conversation (see [`composer_clearance`]).
    composer_height: Pixels,
    /// Tracks keyboard focus anywhere inside each sidebar, so a peek holds
    /// open while the user is typing or tabbing in it.
    side_focus: [FocusHandle; 2],
    /// Local user profile (display name/avatar placeholder), persisted outside
    /// project-scoped state so it survives project switches.
    profile: profile::Profile,
    profile_path: std::path::PathBuf,
    profile_editor_open: bool,
    profile_input: Option<Entity<TextInput>>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(store: Entity<ChatStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            cx.set_global(Theme::for_appearance(window.appearance()));
            // Theme is read during render, so every view needs to render again.
            window.refresh();
        });
        // The app-level theme was picked before the window existed; trust the window.
        cx.set_global(Theme::for_appearance(window.appearance()));

        let panels = cx.new(|_| Panels::new());
        let viewport = window.viewport_size();
        panels.update(cx, |p, _| p.layout(viewport, Instant::now()));
        let panels_changed = cx.observe(&panels, |_, _, cx| cx.notify());
        // Re-apply the collapse rules whenever the window is resized.
        let resized = cx.observe_window_bounds(window, |this, window, cx| {
            let viewport = window.viewport_size();
            this.panels.update(cx, |p, cx| {
                if p.layout(viewport, Instant::now()) {
                    cx.notify();
                }
            });
        });
        let side_focus = [cx.focus_handle(), cx.focus_handle()];
        let mut focus_subscriptions = Vec::new();
        for side in [Side::Left, Side::Right] {
            let handle = &side_focus[side as usize];
            focus_subscriptions.push(cx.on_focus_in(handle, window, move |this, _, cx| {
                this.panels
                    .update(cx, |p, _| p.set_held(side, true, Instant::now()));
            }));
            focus_subscriptions.push(cx.on_focus_out(handle, window, move |this, _, _, cx| {
                this.panels.update(cx, |p, cx| {
                    p.set_held(side, false, Instant::now());
                    p.schedule_poll(cx);
                });
            }));
        }
        let search = cx.new(|cx| {
            Search::new(
                store.clone(),
                std::sync::Arc::new(model::SubstringSearch),
                window,
                cx,
            )
        });
        let pane = cx.new(|cx| PaneHost::new(store.clone(), cx));
        let approval_events = cx.subscribe_in(&store, window, {
            let pane = pane.clone();
            move |_, store, event, window, cx| {
                if matches!(
                    event,
                    crate::store::StoreEvent::ApprovalRequested
                        | crate::store::StoreEvent::ApprovalResolved
                ) {
                    if let Some(content) = store.read(cx).pending_approval() {
                        pane.update(cx, |pane, cx| pane.open(content, window, cx));
                    } else {
                        // The displayed approval was resolved outside the pane.
                        pane.update(cx, |pane, cx| pane.close_resolved_approval(window, cx));
                    }
                }
            }
        });
        // Mention and emoji usage, so suggestions remember picks across launches.
        let usage_path = crate::agents::data_dir().join("composer_suggestions.json");
        let composer = cx.new(|cx| Composer::new(store.clone(), window, cx, Some(usage_path)));
        let pane_events =
            cx.subscribe_in(&pane, window, |this, _, event, window, cx| match event {
                PaneEvent::Reply(reply) => this
                    .composer
                    .update(cx, |c, cx| c.set_reply(reply.clone(), window, cx)),
                PaneEvent::ClosedWithoutFocus => {
                    this.composer.update(cx, |c, cx| c.focus(window, cx))
                }
                PaneEvent::ApprovalClosed => {
                    if let Some(content) = this.store.read(cx).pending_approval() {
                        this.pane
                            .update(cx, |pane, cx| pane.open(content, window, cx));
                    }
                }
                // Attachments, reviews and drafts arrive with the agent runtime.
                PaneEvent::Action(_) => {}
            });
        // The pane layer is drawn here, so redraw when it opens or closes.
        let pane_changed = cx.observe(&pane, |_, _, cx| cx.notify());
        // After a jump, hand the keyboard back to the composer.
        let jumped = cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
            SearchEvent::Jumped | SearchEvent::Dismissed => {
                this.composer.update(cx, |c, cx| c.focus(window, cx))
            }
            // Result actions arrive with the command palette.
            SearchEvent::Action { .. } => {}
        });

        // The model picker draws in the pane layer; Agent Status opens it.
        let model_picker = cx.new(|cx| ModelPicker::new(store.clone(), window, cx));
        let picker_changed = cx.observe(&model_picker, |_, _, cx| cx.notify());
        let picker_events =
            cx.subscribe_in(
                &model_picker,
                window,
                |this, _, event, window, cx| match event {
                    ModelPickerEvent::Dismissed => {
                        this.composer.update(cx, |c, cx| c.focus(window, cx))
                    }
                },
            );

        // A launch-time project identity question (see `project.rs`) opens
        // straight away; the project shown until it's answered is the one
        // this folder had before.
        if let Some(question) = store.read(cx).pending_identity() {
            pane.update(cx, |p, cx| p.open(question, window, cx));
        }

        let data_dir = crate::agents::data_dir();
        let profile_path = profile::path(&data_dir);
        let loaded_profile = profile::load(&data_dir);

        Self {
            store: store.clone(),
            titlebar: cx.new(|cx| Titlebar::new(panels.clone(), search.clone(), window, cx)),
            panels,
            search,
            sidebar: cx.new(|cx| Sidebar::new(store.clone(), pane.clone(), cx)),
            right_sidebar: cx.new(|cx| RightSidebar::new(store.clone(), model_picker.clone(), cx)),
            model_picker,
            conversation: cx
                .new(|cx| Conversation::new(store.clone(), pane.clone(), composer.clone(), cx)),
            pane,
            composer,
            composer_height: COMPOSER_ONE_LINE,
            side_focus,
            profile: loaded_profile,
            profile_path,
            profile_editor_open: false,
            profile_input: None,
            _subscriptions: [
                appearance,
                panels_changed,
                resized,
                jumped,
                pane_events,
                pane_changed,
                picker_changed,
                picker_events,
                approval_events,
            ]
            .into_iter()
            .chain(focus_subscriptions)
            .collect(),
        }
    }
}

/// Wraps a child view so it only re-renders when it is notified itself, not every
/// time something else in the window changes (an animation, a hover elsewhere).
fn cached<V: 'static + gpui::Render>(view: &Entity<V>) -> AnyView {
    AnyView::from(view.clone()).cached(StyleRefinement::default().size_full())
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let viewport = window.viewport_size();

        // Docked, collapsed or peeking, and how far through a slide.
        let ([left, right], sliding) = self.panels.read(cx).frame(Instant::now());
        if sliding {
            window.request_animation_frame();
        }

        let left_w = self.panels.read(cx).width(Side::Left);
        let right_w = self.panels.read(cx).width(Side::Right);
        let left_space = (SIDE_INSET + left_w) * left.reserve;
        let right_space = (right_w + SIDE_INSET) * right.reserve;
        let (pad_l, pad_r, column_w) = centre_column(viewport.width, left_space, right_space);

        let decorations = window.window_decorations();
        let clearance = composer_clearance(self.composer_height);

        div()
            .id("workspace")
            .key_context("Workspace")
            .on_action(cx.listener(|this, _: &ToggleLeftSidebar, _, cx| {
                this.panels.update(cx, |p, cx| {
                    p.toggle(Side::Left, Instant::now());
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ToggleRightSidebar, _, cx| {
                this.panels.update(cx, |p, cx| {
                    p.toggle(Side::Right, Instant::now());
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                // One pane at a time: search takes over from an open pane.
                this.pane.update(cx, |p, cx| p.close_quietly(cx));
                this.search.update(cx, |s, cx| s.focus(window, cx));
            }))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(t.canvas)
            .font_family(FONT_FAMILY)
            .type_style(text::BODY)
            .text_color(t.ink)
            .child(
                div()
                    .h(TITLEBAR_HEIGHT)
                    .flex_none()
                    .child(cached(&self.titlebar)),
            )
            .child(self.profile_button(window, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .pl(pad_l)
                            .pr(pad_r)
                            // Bottom-aligned list: when it was at the newest
                            // message it stays there as this changes.
                            .pb(clearance)
                            .flex()
                            .justify_center()
                            .child(
                                div()
                                    .h_full()
                                    .w_full()
                                    .max_w(column_w)
                                    .child(cached(&self.conversation)),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom(SIDE_INSET)
                            .pl(pad_l)
                            .pr(pad_r)
                            .flex()
                            .justify_center()
                            .child(
                                div()
                                    .relative()
                                    .w_full()
                                    .max_w(column_w)
                                    .child(self.composer.clone())
                                    .child(self.composer_measure(cx)),
                            ),
                    )
                    // The pane: between the docked sidebars, under the
                    // titlebar, above the composer. Peeking sidebars (below)
                    // still slide in over it.
                    .children(self.pane_layer(
                        viewport.width,
                        left_space,
                        right_space,
                        column_w,
                        window,
                        cx,
                    ))
                    // Collapsed sidebars slide off their edge and fade; a peek
                    // slides one back over the content.
                    .when(left.shown > 0., |this| {
                        this.child(self.side_slot(
                            Side::Left,
                            left,
                            left_w,
                            cached(&self.sidebar),
                            cx,
                        ))
                    })
                    .when(right.shown > 0., |this| {
                        this.child(self.side_slot(
                            Side::Right,
                            right,
                            right_w,
                            cached(&self.right_sidebar),
                            cx,
                        ))
                    })
                    .children(self.profile_editor_overlay(right, window, cx)),
            )
            // Edge hover zones: watch every pointer move (capture phase, so
            // nothing on top can swallow it) and let `Panels` decide.
            .child(self.pointer_watcher(cx))
            // Open search floats over everything, grown out of the titlebar
            // pill: same top and centre, so it reads as the pill expanding.
            .child(
                div()
                    .absolute()
                    .top((TITLEBAR_HEIGHT - PILL_HEIGHT) / 2.)
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(self.search.clone()),
            )
            .map(|this| match decorations {
                Decorations::Server => this,
                Decorations::Client { tiling } => {
                    let this = this.when(!tiling.is_tiled(), |this| {
                        this.border_1().border_color(t.line_strong)
                    });
                    if tiling.is_tiled() || window.is_maximized() {
                        this
                    } else {
                        this.children(resize_handles())
                    }
                }
            })
    }
}

impl Workspace {
    /// The open pane (search in pane mode, or whatever `PaneHost` shows) in
    /// the box it may use. Its scrim covers that box, not the sidebars or the
    /// composer, so both stay one click away.
    fn pane_layer(
        &self,
        viewport: Pixels,
        left_space: Pixels,
        right_space: Pixels,
        column: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let (left, right, max_width) = pane_frame(viewport, left_space, right_space, column);
        let pane = self
            .model_picker
            .update(cx, |p, cx| p.render_pane(max_width, window, cx))
            .or_else(|| {
                self.search
                    .update(cx, |s, cx| s.render_pane(max_width, window, cx))
            })
            .or_else(|| {
                self.pane
                    .update(cx, |p, cx| p.render_overlay(max_width, window, cx))
            })?;
        Some(
            div()
                .absolute()
                .top_0()
                .left(left)
                .right(right)
                .bottom(composer_clearance(self.composer_height))
                .child(pane),
        )
    }

    fn toggle_profile_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.profile_editor_open = !self.profile_editor_open;
        if self.profile_editor_open {
            let input = cx.new(|cx| TextInput::new("Your name", cx));
            let name = self.profile.name.clone();
            input.update(cx, |input, cx| {
                if !name.is_empty() {
                    input.restore_draft_state(name.into(), 0..0, false, cx);
                }
            });
            let focus = input.focus_handle(cx);
            window.focus(&focus);
            self.profile_input = Some(input);
        } else {
            self.profile_input = None;
        }
        cx.notify();
    }

    fn save_profile_name(&mut self, cx: &mut Context<Self>) {
        let name = self
            .profile_input
            .as_ref()
            .map(|i| i.read(cx).text().to_string())
            .unwrap_or_default();
        self.profile.name = name.trim().to_string();
        profile::save_in_background(
            self.profile_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .to_path_buf()
                .into(),
            self.profile.clone(),
        );
        cx.notify();
    }

    fn profile_button(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let client_decorations = matches!(window.window_decorations(), Decorations::Client { .. });
        let right_x = if client_decorations {
            space::X3 + px(96.) // WINDOW_CONTROLS_WIDTH
        } else {
            space::X3
        };
        let open = self.profile_editor_open;
        let monogram = self.profile.monogram();
        let seed = self.profile.color_seed();
        let hue = (seed % 360) as f32 / 360.0;
        let avatar_bg = gpui::hsla(hue, 0.45, 0.38, 1.0);
        let hover = t.hover;

        div()
            .id("profile-button")
            .absolute()
            .top(space::X3)
            .right(right_x)
            .size(size::BUTTON_SM)
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .bg(avatar_bg)
            .when(open, |d| d.border_2().border_color(t.accent))
            .hover(move |s| s.bg(if open { avatar_bg } else { hover }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_profile_editor(window, cx);
            }))
            .tooltip(Tooltip::text("Profile"))
            .child(
                div()
                    .text_color(gpui::white())
                    .text_size(px(11.))
                    .font_weight(text::SEMIBOLD)
                    .child(monogram),
            )
    }

    fn profile_editor_overlay(
        &self,
        right_frame: SideFrame,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if !self.profile_editor_open || right_frame.shown <= 0.0 {
            return None;
        }
        let t = *cx.theme();
        let right_w = self.panels.read(cx).width(Side::Right);
        let offset = SIDE_INSET - (right_w + SIDE_INSET) * (1. - right_frame.shown);
        let monogram = self.profile.monogram();
        let seed = self.profile.color_seed();
        let hue = (seed % 360) as f32 / 360.0;
        let avatar_bg = gpui::hsla(hue, 0.45, 0.38, 1.0);
        let name_display = self.profile.display_name().to_string();
        let name_is_empty = self.profile.name.trim().is_empty();
        let input = self.profile_input.clone();

        let overlay =
            div()
                .id("profile-editor-overlay")
                .absolute()
                .right(offset)
                .w(right_w)
                .top(SIDE_TOP)
                .bottom(SIDE_INSET)
                .occlude()
                .rounded(SIDE_RADIUS)
                .bg(t.surface)
                .border_1()
                .border_color(t.line)
                .shadow(t.elevation(Elevation::Overlay))
                .p(space::X4)
                .flex()
                .flex_col()
                .gap(space::X3)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .type_style(text::SMALL)
                                .font_weight(text::SEMIBOLD)
                                .text_color(t.ink)
                                .child("Profile"),
                        )
                        .child(
                            div()
                                .id("profile-close")
                                .size(size::BUTTON_SM)
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(radius::MD)
                                .cursor_pointer()
                                .hover(|s| s.bg(t.hover))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.profile_editor_open = false;
                                    this.profile_input = None;
                                    cx.notify();
                                }))
                                .child(icon(IconName::X, icons::SM, t.muted)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::X3)
                        .child(
                            div()
                                .size(px(40.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(avatar_bg)
                                .child(
                                    div()
                                        .text_color(gpui::white())
                                        .text_size(px(16.))
                                        .font_weight(text::SEMIBOLD)
                                        .child(monogram),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(
                                    div()
                                        .type_style(text::BODY)
                                        .font_weight(text::SEMIBOLD)
                                        .text_color(t.ink)
                                        .child(if name_is_empty {
                                            "Set your name".to_string()
                                        } else {
                                            name_display
                                        }),
                                )
                                .child(div().type_style(text::SMALL).text_color(t.muted).child(
                                    "PFP placeholder · shown in chat/mentions (wiring next)",
                                )),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(space::X2)
                        .child(
                            div()
                                .type_style(text::SMALL)
                                .text_color(t.ink_soft)
                                .child("Display name"),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(space::X2)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .h(px(32.))
                                        .px(space::X2)
                                        .flex()
                                        .items_center()
                                        .rounded(radius::MD)
                                        .bg(t.surface)
                                        .border_1()
                                        .border_color(t.line)
                                        .when_some(input.clone(), |d, input| d.child(input)),
                                )
                                .child(
                                    div()
                                        .id("profile-save")
                                        .h(px(32.))
                                        .px(space::X3)
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(radius::MD)
                                        .bg(t.selected)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(t.hover))
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            this.save_profile_name(cx);
                                            this.profile_editor_open = false;
                                            this.profile_input = None;
                                            cx.notify();
                                        }))
                                        .child(
                                            div()
                                                .type_style(text::SMALL)
                                                .text_color(t.ink)
                                                .child("Save"),
                                        ),
                                ),
                        ),
                );

        Some(overlay)
    }

    /// An invisible canvas laid over the composer that reads its height on
    /// every frame. When the height changes (more lines, a reply chip), the
    /// workspace re-renders on the next frame with the new clearance.
    fn composer_measure(&self, cx: &Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let current = self.composer_height;
        canvas(
            move |bounds, window, cx| {
                let height = bounds.size.height;
                if composer_height_changed(current, height) {
                    // Views can't be invalidated mid-draw, so apply it after.
                    window.defer(cx, move |_, cx| {
                        workspace
                            .update(cx, |this, cx| {
                                if composer_height_changed(this.composer_height, height) {
                                    this.composer_height = height;
                                    cx.notify();
                                }
                            })
                            .ok();
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// One sidebar, docked or peeking. A peek floats over the chat with a
    /// deeper shadow and swallows clicks so nothing underneath reacts.
    fn side_slot(
        &self,
        side: Side,
        frame: SideFrame,
        width: Pixels,
        view: AnyView,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let t = *cx.theme();
        let offset = SIDE_INSET - (width + SIDE_INSET) * (1. - frame.shown);
        let panels = self.panels.clone();
        let handle = div()
            .id(match side {
                Side::Left => "left-sidebar-resize",
                Side::Right => "right-sidebar-resize",
            })
            .absolute()
            .top_0()
            .bottom_0()
            .w(RESIZE_HANDLE)
            .cursor(CursorStyle::ResizeLeftRight)
            .map(|d| match side {
                Side::Left => d.right(px(-3.)),
                Side::Right => d.left(px(-3.)),
            })
            .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                cx.stop_propagation();
                panels.update(cx, |p, cx| {
                    p.start_resize(side, event.position.x);
                    cx.notify();
                });
            });
        div()
            .id(match side {
                Side::Left => "left-sidebar",
                Side::Right => "right-sidebar",
            })
            .absolute()
            .map(|d| match side {
                Side::Left => d.left(offset),
                Side::Right => d.right(offset),
            })
            .w(width)
            .top(SIDE_TOP)
            .bottom(SIDE_INSET)
            .opacity(frame.shown)
            .track_focus(&self.side_focus[side as usize])
            // Clicking a sidebar must not move keyboard focus to the panel
            // itself (the composer keeps it); fields inside still focus
            // themselves. Runs after the children's handlers, before the
            // panel's own focus transfer.
            .on_any_mouse_down(|_, window, _| window.prevent_default())
            .when(frame.overlay, |d| {
                d.occlude()
                    .rounded(SIDE_RADIUS)
                    .shadow(t.elevation(Elevation::Overlay))
            })
            .child(view)
            .child(handle)
    }

    /// An invisible full-window canvas that feeds pointer moves and window
    /// exits to `Panels` for the edge peek and sidebar resize drags.
    fn pointer_watcher(&self, _: &Context<Self>) -> impl IntoElement {
        let panels = self.panels.clone();
        canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let on_move = panels.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    on_move.update(cx, |p, cx| {
                        let changed =
                            p.pointer(Some(e.position), e.pressed_button.is_some(), Instant::now());
                        p.schedule_poll(cx);
                        if changed {
                            cx.notify();
                        }
                    });
                });
                let on_up = panels.clone();
                window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    on_up.update(cx, |p, cx| {
                        if p.resizing() {
                            p.end_resize();
                            cx.notify();
                        }
                    });
                });
                let on_exit = panels.clone();
                window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    on_exit.update(cx, |p, cx| {
                        if p.resizing() {
                            p.end_resize();
                        }
                        p.pointer(None, false, Instant::now());
                        p.schedule_poll(cx);
                        cx.notify();
                    });
                });
            },
        )
        .absolute()
        .size_full()
    }
}

/// Client-side decorations (Linux): thin strips along each edge and corner that
/// start a compositor-driven resize.
fn resize_handles() -> impl Iterator<Item = gpui::AnyElement> {
    const EDGE: Pixels = px(5.);
    const CORNER: Pixels = px(12.);
    let handle = |edge: ResizeEdge| {
        let cursor = match edge {
            ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
            ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
            ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
            ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
        };
        let el = div().absolute().cursor(cursor).occlude();
        let el = match edge {
            ResizeEdge::Top => el.top_0().left(CORNER).right(CORNER).h(EDGE),
            ResizeEdge::Bottom => el.bottom_0().left(CORNER).right(CORNER).h(EDGE),
            ResizeEdge::Left => el.left_0().top(CORNER).bottom(CORNER).w(EDGE),
            ResizeEdge::Right => el.right_0().top(CORNER).bottom(CORNER).w(EDGE),
            ResizeEdge::TopLeft => el.top_0().left_0().size(CORNER),
            ResizeEdge::TopRight => el.top_0().right_0().size(CORNER),
            ResizeEdge::BottomLeft => el.bottom_0().left_0().size(CORNER),
            ResizeEdge::BottomRight => el.bottom_0().right_0().size(CORNER),
        };
        el.on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            window.start_window_resize(edge);
        })
        .into_any_element()
    };
    [
        ResizeEdge::Top,
        ResizeEdge::Bottom,
        ResizeEdge::Left,
        ResizeEdge::Right,
        ResizeEdge::TopLeft,
        ResizeEdge::TopRight,
        ResizeEdge::BottomLeft,
        ResizeEdge::BottomRight,
    ]
    .into_iter()
    .map(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_line_composer_keeps_the_old_clearance() {
        assert_eq!(composer_clearance(COMPOSER_ONE_LINE), px(112.));
        // Before the first measure (or a zero-height glitch) it never shrinks
        // below the one-line clearance.
        assert_eq!(composer_clearance(px(0.)), px(112.));
    }

    #[test]
    fn the_clearance_follows_a_growing_or_shrinking_composer() {
        let line = px(22.);
        let four_lines = composer_clearance(COMPOSER_ONE_LINE + line * 3.);
        assert_eq!(four_lines, px(112.) + line * 3.);
        // A reply chip (about 34px) adds its height too.
        let with_reply = composer_clearance(COMPOSER_ONE_LINE + px(34.));
        assert_eq!(with_reply, px(146.));
        // Shrinking back to one line gives the old clearance again.
        assert_eq!(composer_clearance(COMPOSER_ONE_LINE), px(112.));
    }

    #[test]
    fn sub_pixel_jitter_does_not_move_the_clearance() {
        assert!(!composer_height_changed(px(82.), px(82.3)));
        assert!(composer_height_changed(px(82.), px(104.)));
        assert!(composer_height_changed(px(104.), px(82.)));
    }

    #[test]
    fn column_centres_when_there_is_room() {
        let (l, r, w) = centre_column(px(1920.), px(270.), px(332.));
        assert_eq!(l, r);
        assert_eq!(w, COLUMN_MAX);
    }

    #[test]
    fn column_uses_leftover_space_when_centring_would_squeeze_it() {
        let (l, r, w) = centre_column(px(1440.), px(270.), px(332.));
        assert!(l < r);
        assert!(px(1440.) - l - r <= w);
    }

    #[test]
    fn column_widens_with_the_window_within_bounds() {
        assert_eq!(centre_column(px(800.), px(0.), px(0.)).2, COLUMN_MIN);
        assert_eq!(centre_column(px(3000.), px(0.), px(0.)).2, COLUMN_MAX);
    }
}
