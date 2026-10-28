//! What the pane can show, without any UI.
//!
//! The pane is the glass overlay that shows what you need or what you clicked
//! on (a long message, an attachment, settings, a review, a long message you
//! are writing) without taking you away from the chat behind it. The UI draws
//! a [`PaneContent`]; anything that can build one (a click in the chat today,
//! the agent runtime later) can open a pane.
//!
//! Long messages are also decided here: [`is_long`] says when a message is
//! collapsed in the chat and [`preview`] is the part shown before "Open".

use std::sync::Arc;

use crate::{Author, Id, Message, SearchResult, Workspace};

/// A message is collapsed in the chat when it has more characters than this…
pub const LONG_MESSAGE_CHARS: usize = 600;
/// …or more lines than this.
pub const LONG_MESSAGE_LINES: usize = 8;
/// The collapsed preview keeps at most this many characters…
pub const PREVIEW_CHARS: usize = 320;
/// …and this many lines.
pub const PREVIEW_LINES: usize = 4;

/// Whether `body` is long enough to be collapsed in the chat.
pub fn is_long(body: &str) -> bool {
    body.lines().count() > LONG_MESSAGE_LINES || body.chars().count() > LONG_MESSAGE_CHARS
}

/// The start of a long message for its collapsed form: at most
/// [`PREVIEW_LINES`] lines and [`PREVIEW_CHARS`] characters, cut at a word
/// boundary when one is near, ending in "…". Short messages come back whole.
pub fn preview(body: &str) -> String {
    if !is_long(body) {
        return body.to_string();
    }
    let mut out = String::new();
    let mut chars = 0;
    for (ix, line) in body.lines().take(PREVIEW_LINES).enumerate() {
        if ix > 0 {
            out.push('\n');
            chars += 1;
        }
        for ch in line.chars() {
            if chars == PREVIEW_CHARS {
                return finish_cut(out);
            }
            out.push(ch);
            chars += 1;
        }
        if chars >= PREVIEW_CHARS {
            return finish_cut(out);
        }
    }
    let mut out = out.trim_end().to_string();
    out.push('…');
    out
}

/// Back off to the last space when it is close, so the cut lands between words.
fn finish_cut(mut out: String) -> String {
    let tail_start = out.char_indices().rev().nth(40).map_or(0, |(ix, _)| ix);
    if let Some(space) = out[tail_start..].rfind(char::is_whitespace) {
        out.truncate(tail_start + space);
    }
    let mut out = out.trim_end().to_string();
    out.push('…');
    out
}

/// One line quoting a message, for reply references: the first line of the
/// body, collapsed whitespace, at most `max` characters with "…".
pub fn quote_line(body: &str, max: usize) -> Arc<str> {
    let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let words: Vec<&str> = first.split_whitespace().collect();
    let line = words.join(" ");
    if line.chars().count() <= max {
        let more = body.trim().lines().count() > 1;
        return if more {
            Arc::from(format!("{line}…"))
        } else {
            Arc::from(line)
        };
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    let cut = match cut.rfind(' ') {
        Some(space) if space > cut.len() / 2 => cut[..space].to_string(),
        _ => cut,
    };
    Arc::from(format!("{}…", cut.trim_end()))
}

/// What a reply points at. A snapshot rather than a lookup, so drawing a
/// reply in a 100k-message channel never searches the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyRef {
    pub message: Id,
    pub author: Author,
    /// One line of the original (see [`quote_line`]).
    pub quote: Arc<str>,
}

/// Characters a reply reference quotes.
pub const REPLY_QUOTE_CHARS: usize = 90;

impl ReplyRef {
    pub fn to(message: &Message) -> Self {
        Self {
            message: message.id,
            author: message.author,
            quote: quote_line(&message.body, REPLY_QUOTE_CHARS),
        }
    }
}

/// A file someone attached. The bytes live elsewhere; the pane shows what it is
/// and a text preview when there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub name: Arc<str>,
    pub mime: Arc<str>,
    pub size_bytes: u64,
    /// Text to show in the pane (a log, a diff, a snippet), if any.
    pub preview: Option<Arc<str>>,
}

/// One thing to look at in a review (a file, a change, a finding).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewItem {
    pub title: Arc<str>,
    pub detail: Arc<str>,
}

/// Everything the pane can show. Each variant knows its title and its quick
/// actions; the UI decides how the body looks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneContent {
    /// A message too long for the chat, in full.
    LongMessage {
        channel: Id,
        /// Position in the channel log (logs are append-only, so it is stable).
        index: usize,
        message: Message,
    },
    /// An attachment to look at before sending it on or adding it to the chat.
    Attachment(Attachment),
    /// App settings.
    Settings,
    /// Something an agent wants reviewed, with its findings.
    Review {
        title: Arc<str>,
        summary: Arc<str>,
        items: Vec<ReviewItem>,
    },
    /// A long message being written for `channel`.
    Compose { channel: Id, draft: Arc<str> },
    /// Search results (the search-in-pane experiment).
    Search {
        query: Arc<str>,
        results: Vec<SearchResult>,
        highlighted: usize,
    },
    /// The launch folder's `.chaos/project.toml` disagrees with what the app
    /// remembers about the folder. Nothing switches until the user answers.
    ProjectIdentity(ProjectQuestion),
    /// An agent on this computer asked for permission. It is not waiting on
    /// the answer; Approve or Deny reaches it as a message.
    Permission(PermissionPrompt),
}

/// A pending permission request, as the pane shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionPrompt {
    /// Core's request id (`#N` in the chat).
    pub request: i64,
    /// The agent's display name.
    pub agent: Arc<str>,
    /// What it wants to do (the tool call's title).
    pub action: Arc<str>,
    /// Why, in its words, when it said.
    pub reason: Option<Arc<str>>,
    /// The tool's raw input, when there is one.
    pub detail: Option<Arc<str>>,
    /// Who it was sent to ("Hx"); any human here can answer.
    pub asked: Arc<str>,
}

/// What the project-identity pane asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectQuestion {
    pub kind: ProjectQuestionKind,
    /// What is open now.
    pub current: Arc<str>,
    /// What the file names, and its id.
    pub proposed: Arc<str>,
    pub proposed_id: Arc<str>,
}

/// Which identity question it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectQuestionKind {
    /// A folder the app knows now names a different project.
    Changed,
    /// The file names a project open from this path, which still claims it,
    /// so this folder is a copy.
    CopiedFrom(Arc<str>),
    /// A new folder, while history from before projects is still unclaimed.
    /// `current` is the fresh project; `proposed` is that history, under the
    /// name it takes if adopted.
    Imported,
}

/// The quick actions panes offer. `Custom` carries an id the opener handles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneActionKind {
    /// Put a reply reference to the message into the composer.
    Reply,
    /// Copy the message text.
    Copy,
    /// Add the thing in the pane to the current chat.
    AddToChat,
    Approve,
    RequestChanges,
    Send,
    Discard,
    /// Jump to the highlighted search result.
    OpenResult,
    /// One of a search result's own actions (`SearchAction::id`).
    Custom(Arc<str>),
    /// Project identity: keep what is open and make the folder's file say so.
    KeepProject,
    /// Project identity: open the project the file names.
    UseProposedProject,
    /// Project identity: start a new project for this folder.
    NewProject,
    /// Project identity: adopt the unclaimed pre-project history here.
    AdoptImported,
    /// Permission request: say no (Approve says yes).
    Deny,
}

/// A full-width row along the bottom of the pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneAction {
    pub kind: PaneActionKind,
    pub label: Arc<str>,
}

impl PaneAction {
    pub fn new(kind: PaneActionKind, label: &str) -> Self {
        Self {
            kind,
            label: Arc::from(label),
        }
    }
}

impl PaneContent {
    /// The large title next to the close dot.
    pub fn title(&self, ws: &Workspace) -> Arc<str> {
        match self {
            PaneContent::LongMessage {
                channel, message, ..
            } => {
                let who = author_name(ws, message.author);
                let channel = ws
                    .channel(*channel)
                    .map_or("unknown".into(), |c| c.name.clone());
                Arc::from(format!("{who} in #{channel}"))
            }
            PaneContent::Attachment(a) => a.name.clone(),
            PaneContent::Settings => Arc::from("Settings"),
            PaneContent::Review { title, .. } => title.clone(),
            PaneContent::Compose { channel, .. } => {
                let channel = ws
                    .channel(*channel)
                    .map_or("unknown".into(), |c| c.name.clone());
                Arc::from(format!("Message #{channel}"))
            }
            PaneContent::Search { .. } => Arc::from("Search"),
            PaneContent::ProjectIdentity(q) => Arc::from(match q.kind {
                ProjectQuestionKind::Changed => "This folder's project changed",
                ProjectQuestionKind::CopiedFrom(_) => "This folder looks like a copy",
                ProjectQuestionKind::Imported => "Earlier history found",
            }),
            PaneContent::Permission(p) => Arc::from(format!("{} asks permission", p.agent)),
        }
    }

    /// Quick actions in the order they are drawn, top to bottom.
    pub fn actions(&self) -> Vec<PaneAction> {
        use PaneActionKind::*;
        match self {
            PaneContent::LongMessage { .. } => vec![
                PaneAction::new(Reply, "Reply"),
                PaneAction::new(Copy, "Copy"),
            ],
            PaneContent::Attachment(_) => vec![PaneAction::new(AddToChat, "Add to chat")],
            PaneContent::Settings => Vec::new(),
            PaneContent::Review { .. } => vec![
                PaneAction::new(Approve, "Approve"),
                PaneAction::new(RequestChanges, "Request changes"),
            ],
            PaneContent::Compose { .. } => vec![
                PaneAction::new(Send, "Send"),
                PaneAction::new(Discard, "Discard draft"),
            ],
            PaneContent::ProjectIdentity(q) => match q.kind {
                ProjectQuestionKind::Changed => vec![
                    PaneAction::new(KeepProject, "Keep current"),
                    PaneAction::new(UseProposedProject, "Use the new one"),
                    PaneAction::new(NewProject, "Treat as a new project"),
                ],
                ProjectQuestionKind::CopiedFrom(_) => vec![
                    PaneAction::new(KeepProject, "Keep it separate"),
                    PaneAction {
                        kind: UseProposedProject,
                        label: Arc::from(format!("Use {} here too", q.proposed)),
                    },
                ],
                ProjectQuestionKind::Imported => vec![
                    PaneAction::new(AdoptImported, "Use my earlier history here"),
                    // Keeps the fresh project and writes its file.
                    PaneAction::new(KeepProject, "Start fresh"),
                ],
            },
            PaneContent::Permission(_) => vec![
                PaneAction::new(Approve, "Approve"),
                PaneAction::new(Deny, "Deny"),
            ],
            PaneContent::Search {
                results,
                highlighted,
                ..
            } => match results.get(*highlighted) {
                None => Vec::new(),
                Some(result) => {
                    let open = match result.hit {
                        crate::SearchHit::Channel { .. } => "Go to channel",
                        crate::SearchHit::Message { .. } => "Go to message",
                    };
                    std::iter::once(PaneAction::new(OpenResult, open))
                        .chain(result.actions.iter().map(|a| PaneAction {
                            kind: Custom(a.id.clone()),
                            label: a.label.clone(),
                        }))
                        .collect()
                }
            },
        }
    }
}

/// Display name for a message author.
pub fn author_name(ws: &Workspace, author: Author) -> Arc<str> {
    match author {
        Author::Human => Arc::from("Hx"),
        Author::System => Arc::from("Chaos Control"),
        Author::Agent(id) => ws.agent(id).map_or(Arc::from("Agent"), |a| a.name.clone()),
    }
}

/// App settings. Not persisted yet; there is no storage layer.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Settings {
    /// Experiment: search results open in the pane instead of the dropdown.
    /// Off by default.
    pub search_in_pane: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SearchAction, SearchHit, sample};

    fn message(body: &str) -> Message {
        Message {
            id: Id(7),
            author: Author::Agent(Id(11)),
            sent_at: 0,
            body: Arc::from(body),
            reply_to: None,
            stance: None,
        }
    }

    #[test]
    fn permission_offers_approve_and_deny() {
        let pane = PaneContent::Permission(PermissionPrompt {
            request: 3,
            agent: Arc::from("Claude Code"),
            action: Arc::from("rm -rf build/"),
            reason: None,
            detail: None,
            asked: Arc::from("Hx"),
        });
        assert_eq!(&*pane.title(&sample()), "Claude Code asks permission");
        let kinds: Vec<_> = pane.actions().into_iter().map(|a| a.kind).collect();
        assert_eq!(kinds, vec![PaneActionKind::Approve, PaneActionKind::Deny]);
    }

    #[test]
    fn project_identity_offers_the_three_answers() {
        let q = ProjectQuestion {
            kind: ProjectQuestionKind::Changed,
            current: Arc::from("starpoint"),
            proposed: Arc::from("everything"),
            proposed_id: Arc::from("5f0c9d1a-7b2e-4e8f-a1c3-9d8e7f6a5b4c"),
        };
        let pane = PaneContent::ProjectIdentity(q.clone());
        let labels: Vec<_> = pane.actions().into_iter().map(|a| a.label).collect();
        assert_eq!(
            labels,
            vec![
                Arc::from("Keep current"),
                Arc::from("Use the new one"),
                Arc::from("Treat as a new project")
            ]
        );
        let copy = PaneContent::ProjectIdentity(ProjectQuestion {
            kind: ProjectQuestionKind::CopiedFrom(Arc::from("~/src/everything")),
            ..q.clone()
        });
        assert_eq!(copy.actions().len(), 2);
        assert_eq!(&*copy.title(&sample()), "This folder looks like a copy");
        let imported = PaneContent::ProjectIdentity(ProjectQuestion {
            kind: ProjectQuestionKind::Imported,
            ..q
        });
        let actions = imported.actions();
        assert_eq!(
            actions.iter().map(|a| a.label.clone()).collect::<Vec<_>>(),
            vec![
                Arc::from("Use my earlier history here"),
                Arc::from("Start fresh")
            ]
        );
        assert_eq!(actions[0].kind, PaneActionKind::AdoptImported);
        assert_eq!(actions[1].kind, PaneActionKind::KeepProject);
        assert_eq!(&*imported.title(&sample()), "Earlier history found");
    }

    #[test]
    fn short_messages_are_not_collapsed() {
        assert!(!is_long("Pushed the fix. CI is green on my side."));
        let at_limit = "a".repeat(LONG_MESSAGE_CHARS);
        assert!(!is_long(&at_limit));
        let lines = ["line"; LONG_MESSAGE_LINES].join("\n");
        assert!(!is_long(&lines));
        assert_eq!(preview("short"), "short");
    }

    #[test]
    fn long_by_characters_or_by_lines() {
        assert!(is_long(&"a".repeat(LONG_MESSAGE_CHARS + 1)));
        let lines = ["- x"; LONG_MESSAGE_LINES + 1].join("\n");
        assert!(is_long(&lines));
    }

    #[test]
    fn character_count_not_bytes() {
        // 600 two-byte characters are 1200 bytes but not long.
        assert!(!is_long(&"é".repeat(LONG_MESSAGE_CHARS)));
    }

    #[test]
    fn preview_keeps_the_first_lines() {
        let body = (1..=12)
            .map(|i| format!("Line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let p = preview(&body);
        assert_eq!(p, "Line 1\nLine 2\nLine 3\nLine 4…");
    }

    #[test]
    fn preview_cuts_long_prose_between_words() {
        let body = "word ".repeat(400);
        let p = preview(&body);
        assert!(p.ends_with("word…"), "{p}");
        assert!(p.chars().count() <= PREVIEW_CHARS + 1);
        assert!(p.chars().count() > PREVIEW_CHARS - 45);
    }

    #[test]
    fn preview_never_splits_a_character() {
        let body = "日本語".repeat(400);
        let p = preview(&body);
        assert!(p.ends_with('…'));
        assert_eq!(p.chars().count(), PREVIEW_CHARS + 1);
    }

    #[test]
    fn quote_line_is_one_line() {
        assert_eq!(&*quote_line("Hello   there", 90), "Hello there");
        assert_eq!(&*quote_line("\n\nFirst\nSecond", 90), "First…");
        let long = "alpha beta gamma delta epsilon zeta eta theta";
        let q = quote_line(long, 20);
        assert!(q.chars().count() <= 20, "{q}");
        assert!(q.ends_with('…'));
        assert!(!q.contains("  "));
    }

    #[test]
    fn reply_ref_snapshots_the_message() {
        let m = message("Can you review the migration?\nIt drops a column.");
        let r = ReplyRef::to(&m);
        assert_eq!(r.message, Id(7));
        assert_eq!(r.author, Author::Agent(Id(11)));
        assert_eq!(&*r.quote, "Can you review the migration?…");
    }

    #[test]
    fn long_message_pane_has_title_and_reply_copy() {
        let ws = sample();
        let content = PaneContent::LongMessage {
            channel: ws.channels[0].id,
            index: 3,
            message: message("…"),
        };
        assert_eq!(&*content.title(&ws), "Claude Code in #main");
        let kinds: Vec<_> = content.actions().into_iter().map(|a| a.kind).collect();
        assert_eq!(kinds, vec![PaneActionKind::Reply, PaneActionKind::Copy]);
    }

    #[test]
    fn settings_pane_has_no_quick_actions() {
        let ws = sample();
        assert_eq!(&*PaneContent::Settings.title(&ws), "Settings");
        assert!(PaneContent::Settings.actions().is_empty());
        assert!(!Settings::default().search_in_pane, "experiment is off");
    }

    #[test]
    fn search_pane_reuses_result_actions() {
        let ws = sample();
        let hit = SearchHit::Channel {
            channel: ws.channels[1].id,
        };
        let result = SearchResult {
            hit,
            actions: vec![SearchAction {
                id: Arc::from("new-chat"),
                label: Arc::from("New chat"),
            }],
        };
        let content = PaneContent::Search {
            query: Arc::from("strm"),
            results: vec![result],
            highlighted: 0,
        };
        let actions = content.actions();
        assert_eq!(actions[0].kind, PaneActionKind::OpenResult);
        assert_eq!(&*actions[0].label, "Go to channel");
        assert_eq!(
            actions[1].kind,
            PaneActionKind::Custom(Arc::from("new-chat"))
        );
        let empty = PaneContent::Search {
            query: Arc::from("zzz"),
            results: Vec::new(),
            highlighted: 0,
        };
        assert!(empty.actions().is_empty());
    }

    #[test]
    fn every_kind_the_runtime_needs_has_a_title() {
        let ws = sample();
        let kinds = [
            PaneContent::Attachment(Attachment {
                name: Arc::from("crash.log"),
                mime: Arc::from("text/plain"),
                size_bytes: 2048,
                preview: None,
            }),
            PaneContent::Review {
                title: Arc::from("Review: sync PR"),
                summary: Arc::from("Three blocking comments."),
                items: Vec::new(),
            },
            PaneContent::Compose {
                channel: ws.channels[0].id,
                draft: Arc::from(""),
            },
        ];
        let titles: Vec<_> = kinds.iter().map(|k| k.title(&ws).to_string()).collect();
        assert_eq!(titles, ["crash.log", "Review: sync PR", "Message #main"]);
        assert!(kinds.iter().all(|k| !k.actions().is_empty()));
    }
}
