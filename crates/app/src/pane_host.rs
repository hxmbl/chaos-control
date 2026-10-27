//! Opens `model::PaneContent` in the glass pane and turns its quick actions
//! into app behaviour.
//!
//! Anything can open a pane through `PaneHost::open`: the chat's "Open" on a
//! long message and the sidebar's Settings today, the agent runtime later.
//! One pane at a time; opening another swaps the content in place. Closing
//! (the close dot, Escape, a click outside) hands the keyboard back to
//! whatever had it before the pane opened, while the pane fades out.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, FontWeight, Pixels,
    SharedString, Task, Window, div, prelude::*, px,
};
use model::{Author, PaneActionKind, PaneContent, ReplyRef, pane::author_name};
use ui::{ActiveTheme, IconName, Pane, PaneState, QuickAction, brand_icon, icon, switch};

use crate::{settings, store::ChatStore};

/// How long "Copy" reads "Copied".
const COPIED_FOR: Duration = Duration::from_millis(1400);

pub enum PaneEvent {
    /// "Reply" on a message: put a reply reference in the composer.
    Reply(ReplyRef),
    /// The pane closed and nothing had focus before it opened, so the
    /// workspace should pick (the composer).
    ClosedWithoutFocus,
    /// A quick action this host doesn't handle itself (attachments, reviews,
    /// drafts); the agent runtime will route these.
    #[allow(dead_code)]
    Action(PaneActionKind),
}

pub struct PaneHost {
    store: Entity<ChatStore>,
    state: PaneState<PaneContent, FocusHandle>,
    focus: FocusHandle,
    copied_at: Option<Instant>,
    _copied_timer: Option<Task<()>>,
}

impl EventEmitter<PaneEvent> for PaneHost {}

impl PaneHost {
    pub fn new(store: Entity<ChatStore>, cx: &mut Context<Self>) -> Self {
        Self {
            store,
            state: PaneState::new(),
            focus: cx.focus_handle(),
            copied_at: None,
            _copied_timer: None,
        }
    }

    /// Shows `content` and takes the keyboard (so Escape closes it),
    /// remembering what had focus to give it back on close.
    pub fn open(&mut self, content: PaneContent, window: &mut Window, cx: &mut Context<Self>) {
        let focused = window
            .focused(cx)
            .filter(|f| !self.focus.contains(f, window));
        self.state.open(content, focused, Instant::now());
        self.copied_at = None;
        window.focus(&self.focus);
        cx.notify();
    }

    /// Closes the pane (it fades out, taking no input) and restores focus. The restore waits a beat and only
    /// happens if the pane still has the keyboard: when a click outside both
    /// closed the pane and focused something (the search pill, the
    /// composer), that click wins.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(restore) = self.state.close(Instant::now()) else {
            return;
        };
        cx.notify();
        cx.defer_in(window, move |this, window, cx| {
            let pane_had_it = this.focus.is_focused(window) || window.focused(cx).is_none();
            if !pane_had_it {
                return;
            }
            match restore {
                Some(handle) => window.focus(&handle),
                None => cx.emit(PaneEvent::ClosedWithoutFocus),
            }
        });
    }

    /// Closes without touching focus, for when something else is taking the
    /// keyboard on purpose (cmd/ctrl-K opening search).
    pub fn close_quietly(&mut self, cx: &mut Context<Self>) {
        if self.state.close(Instant::now()).is_some() {
            cx.notify();
        }
    }

    fn run(&mut self, kind: PaneActionKind, window: &mut Window, cx: &mut Context<Self>) {
        match (&kind, self.state.content()) {
            (PaneActionKind::Reply, Some(PaneContent::LongMessage { message, .. })) => {
                let reply = ReplyRef::to(message);
                self.close(window, cx);
                cx.emit(PaneEvent::Reply(reply));
            }
            (PaneActionKind::Copy, Some(PaneContent::LongMessage { message, .. })) => {
                cx.write_to_clipboard(ClipboardItem::new_string(message.body.to_string()));
                self.copied_at = Some(Instant::now());
                self._copied_timer = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(COPIED_FOR).await;
                    this.update(cx, |this, cx| {
                        this.copied_at = None;
                        cx.notify();
                    })
                    .ok();
                }));
                cx.notify();
            }
            (
                PaneActionKind::KeepProject
                | PaneActionKind::UseProposedProject
                | PaneActionKind::NewProject,
                Some(PaneContent::ProjectIdentity(_)),
            ) => {
                self.store
                    .update(cx, |store, cx| store.answer_identity(&kind, cx));
                self.close(window, cx);
            }
            _ => {
                self.close(window, cx);
                cx.emit(PaneEvent::Action(kind));
            }
        }
    }

    /// The pane, ready to drop into the workspace's pane layer, or `None`
    /// once closed and faded out. `max_width` comes from [`pane_frame`].
    pub fn render_overlay(
        &mut self,
        max_width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = Instant::now();
        let content = self.state.shown(now)?.clone();
        let closing = !self.state.is_open();
        if self.state.animating(now) {
            window.request_animation_frame();
        }
        let title = SharedString::from(content.title(self.store.read(cx).workspace()));
        let copied = self.copied_at.is_some();
        let actions = content
            .actions()
            .into_iter()
            .enumerate()
            .map(|(ix, action)| {
                let label: SharedString = match action.kind {
                    PaneActionKind::Copy if copied => "Copied".into(),
                    _ => action.label.into(),
                };
                let kind = action.kind;
                let this = cx.entity();
                QuickAction::new(("pane-action", ix), label, move |window, cx| {
                    this.update(cx, |this, cx| this.run(kind.clone(), window, cx))
                })
            });
        let close = cx.entity();
        let pane = Pane::new(("pane", self.state.generation() as usize))
            .focus(&self.focus)
            .progress(self.state.progress(now))
            .closing(closing)
            .max_width(max_width)
            .title(title)
            .on_close(move |window, cx| close.update(cx, |this, cx| this.close(window, cx)))
            .actions(actions.collect::<Vec<_>>());
        let pane = match &content {
            PaneContent::LongMessage { message, .. } => pane
                .child(self.byline(message.author, message.sent_at, cx))
                .children(paragraphs(&message.body)),
            PaneContent::Settings => pane.children(self.settings_rows(cx)),
            PaneContent::Attachment(a) => pane
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted)
                        .child(format!("{} · {}", a.mime, human_size(a.size_bytes))),
                )
                .children(a.preview.as_deref().map(paragraphs).unwrap_or_default()),
            PaneContent::Review { summary, items, .. } => pane
                .children(paragraphs(summary))
                .children(items.iter().map(|item| {
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(cx.theme().ink)
                                .child(SharedString::from(item.title.clone())),
                        )
                        .child(SharedString::from(item.detail.clone()))
                })),
            PaneContent::Compose { draft, .. } => pane.children(paragraphs(draft)),
            // Search draws its own pane (see `Search::render_pane`).
            PaneContent::Search { .. } => pane,
            PaneContent::ProjectIdentity(q) => pane.children(paragraphs(&identity_text(q))),
        };
        Some(pane.into_any_element())
    }

    /// "Claude Code · 10:08" with the author's mark, above a long message.
    fn byline(&self, author: Author, sent_at: u64, cx: &Context<Self>) -> AnyElement {
        let t = *cx.theme();
        let ws = self.store.read(cx).workspace();
        let mark = match author {
            Author::Agent(id) => ws
                .agent(id)
                .map(|a| icon(brand_icon(a.brand), px(14.), t.ink_soft).into_any_element()),
            Author::Human | Author::System => None,
        };
        let minutes = sent_at / 60;
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(13.))
            .text_color(t.muted)
            .children(mark)
            .child(SharedString::from(format!(
                "{} · {:02}:{:02}",
                author_name(ws, author),
                (minutes / 60) % 24,
                minutes % 60
            )))
            .into_any_element()
    }

    fn settings_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = *cx.theme();
        let search_in_pane = settings::get(cx).search_in_pane;
        let appearance = if t.is_dark() { "Dark" } else { "Light" };
        let row = |label: &'static str, about: SharedString| {
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .px(px(14.))
                .py(px(10.))
                .rounded(px(14.))
                .bg(t.glass().well)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(14.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(t.ink)
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .line_height(px(19.))
                                .text_color(t.muted)
                                .child(about),
                        ),
                )
        };
        let soon = |d: gpui::Div| {
            d.child(
                div()
                    .flex_none()
                    .px(px(8.))
                    .py(px(2.))
                    .rounded_full()
                    .border_1()
                    .border_color(t.line_strong)
                    .text_size(px(11.5))
                    .text_color(t.muted)
                    .child("Soon"),
            )
        };
        let shortcut = if cfg!(target_os = "macos") {
            "⌘K search · ⌘⌥[ and ⌘⌥] sidebars · Esc closes the pane"
        } else {
            "Ctrl+K search · Ctrl+Alt+[ and Ctrl+Alt+] sidebars · Esc closes the pane"
        };
        vec![
            row(
                "Appearance",
                format!("Follows your system. Currently {appearance}.").into(),
            )
            .into_any_element(),
            row(
                "Search in the pane",
                "Experiment: search results open in this pane instead of the dropdown.".into(),
            )
            .child(
                switch("search-in-pane", search_in_pane, cx).on_click(cx.listener(
                    |_, _, window, cx| {
                        settings::update(cx, |s| s.search_in_pane = !s.search_in_pane);
                        // Search reads the flag while rendering.
                        window.refresh();
                        cx.notify();
                    },
                )),
            )
            .into_any_element(),
            row(
                "Long messages",
                format!(
                    "Messages over {} characters or {} lines show a preview with Open.",
                    model::pane::LONG_MESSAGE_CHARS,
                    model::pane::LONG_MESSAGE_LINES
                )
                .into(),
            )
            .into_any_element(),
            row("Shortcuts", shortcut.into()).into_any_element(),
            soon(row(
                "Agents",
                "Add Claude Code, Codex and others to this project.".into(),
            ))
            .into_any_element(),
            soon(row(
                "Notifications",
                "Mentions, direct messages and replies.".into(),
            ))
            .into_any_element(),
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(4.))
                .text_size(px(12.5))
                .text_color(t.faint)
                .child(icon(IconName::GearSix, px(13.), t.faint))
                .child("Settings aren't saved between launches yet.")
                .into_any_element(),
        ]
    }
}

/// One element per line, with a small gap for blank lines, so bullet lists and
/// paragraphs keep their shape.
fn paragraphs(text: &str) -> Vec<AnyElement> {
    let mut out = Vec::new();
    let mut gap = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            gap = true;
            continue;
        }
        out.push(
            div()
                .w_full()
                .whitespace_normal()
                .when(gap, |d| d.mt(px(6.)))
                .child(SharedString::from(line.to_string()))
                .into_any_element(),
        );
        gap = false;
    }
    out
}

/// The body of the project-identity pane. Says what is open, what the file
/// asks for, and that nothing switched on its own.
fn identity_text(q: &model::ProjectQuestion) -> String {
    let short: String = q.proposed_id.chars().take(8).collect();
    match &q.copied_from {
        None => format!(
            "This folder's .chaos/project.toml now names a different project, {} ({short}…), \
             not {}, the one Chaos Control opened here before.\n\
             {} is still open. Nothing switches until you choose: using the new one shows \
             its channels and history in this folder; a new project starts empty.",
            q.proposed, q.current, q.current
        ),
        Some(path) => format!(
            "This folder's .chaos/project.toml names {} ({short}…), which is open from {path}, \
             and that folder still claims it. So this looks like a copy, and Chaos Control \
             opened a separate project here to keep the two chats apart.\n\
             Use {} here too only if both folders should share one chat on this computer.",
            q.proposed, q.proposed
        ),
    }
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.),
        b => format!("{:.1} MB", b as f64 / (1024. * 1024.)),
    }
}

/// Where the pane goes, horizontally: the left and right edges of the area it
/// may use, and its widest width.
///
/// It stays between docked sidebars (`left_space`/`right_space` are what they
/// take from each edge, sliding with them), centred on the window like the
/// chat column when there is room, and
/// runs a bit wider than the column, as in the mock: column + 160px, at most
/// 1120px. On a narrow window it simply takes the space there is.
pub fn pane_frame(
    viewport: Pixels,
    left_space: Pixels,
    right_space: Pixels,
    column: Pixels,
) -> (Pixels, Pixels, Pixels) {
    let (left, right) = (left_space, right_space);
    let available = (viewport - left - right).max(px(0.));
    let width = (column + px(160.)).min(px(1120.)).min(available);
    // Centred on the window (where the column and the titlebar search are)
    // when that still clears both sidebars.
    let side = left.max(right);
    if viewport - side - side >= width {
        (side, side, width)
    } else {
        (left, right, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_is_wider_than_the_column_when_there_is_room() {
        let (l, r, w) = pane_frame(px(1920.), px(270.), px(332.), px(960.));
        assert_eq!((l, r), (px(332.), px(332.)), "centred under the search");
        assert_eq!(w, px(1120.));
        let (_, _, w) = pane_frame(px(1440.), px(0.), px(0.), px(864.));
        assert_eq!(w, px(1024.));
    }

    #[test]
    fn pane_never_overlaps_docked_sidebars() {
        // Narrow window with the left sidebar pinned: whatever is left over.
        let (l, r, w) = pane_frame(px(760.), px(270.), px(0.), px(720.));
        assert_eq!(l, px(270.));
        assert_eq!(r, px(0.));
        assert_eq!(w, px(490.));
        assert!(l + w + r <= px(760.));
    }

    #[test]
    fn pane_frame_never_goes_negative() {
        let (_, _, w) = pane_frame(px(400.), px(270.), px(332.), px(720.));
        assert_eq!(w, px(0.));
    }

    #[test]
    fn paragraphs_keep_lines_and_drop_blanks() {
        assert_eq!(paragraphs("a\n\nb\n• c").len(), 3);
        assert!(paragraphs("").is_empty());
    }

    #[test]
    fn sizes_read_like_a_file_manager() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
