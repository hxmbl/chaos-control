//! The app's single source of truth for chat state.
//!
//! Views hold an `Entity<ChatStore>` and subscribe to its events instead of
//! sharing data by cloning it around. Each event says exactly what changed, so a
//! view only re-renders (or re-measures list rows) for the part it shows.

use std::{collections::HashMap, path::Path, sync::Arc};

use chaos_core::{engine::CoreEvent, types as ct};
use gpui::{Context, EventEmitter, Task};
use model::{
    Agent, AgentState, Author, ChannelLog, CorpusChannel, Id, Message, PaneActionKind, PaneContent,
    ProjectEntry, ProjectQuestion, ReplyRef, SearchCorpus, Workspace,
};

use crate::{
    agents::{self, CoreLink, OpenProject},
    project::{self, Confirm, Question, Target},
};

/// Model ids for things that come from core sit above these, clear of the
/// sample data and local echoes.
const CORE_AGENT_BASE: u32 = 100_000;
const CORE_CHANNEL_BASE: u32 = 0x2000_0000;
const CORE_MESSAGE_BASE: u32 = 0x4000_0000;

pub enum StoreEvent {
    /// The selected channel changed.
    Selected,
    /// Whole messages were appended to a channel's log at `range`.
    Appended {
        channel: Id,
        range: std::ops::Range<usize>,
    },
    /// Scroll to and highlight a message (from search). Follows `Selected` when
    /// the message is in another channel.
    Reveal { channel: Id, index: usize },
}

pub struct ChatStore {
    workspace: Workspace,
    logs: HashMap<Id, ChannelLog>,
    selected: Id,
    next_message_id: u32,
    core: Option<Attached>,
}

/// Core behind #main (see `agents.rs`).
struct Attached {
    link: CoreLink,
    /// The app channel core's main channel shows in.
    channel: Id,
    /// Core message id -> index in that channel's log (dedups history vs events).
    seen: HashMap<ct::MessageId, usize>,
    /// An unanswered project-identity question (see `project::decide`).
    confirm: Option<Confirm>,
    _events: Task<()>,
}

impl EventEmitter<StoreEvent> for ChatStore {}

impl ChatStore {
    pub fn new(workspace: Workspace) -> Self {
        let selected = workspace.channels[0].id;
        let logs = workspace
            .channels
            .iter()
            .map(|c| (c.id, ChannelLog::default()))
            .collect();
        Self {
            workspace,
            logs,
            selected,
            next_message_id: 1_000_000,
            core: None,
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn selected(&self) -> Id {
        self.selected
    }

    pub fn selected_channel(&self) -> &model::Channel {
        self.workspace
            .channel(self.selected)
            .unwrap_or(&self.workspace.channels[0])
    }

    pub fn log(&self, channel: Id) -> Option<&ChannelLog> {
        self.logs.get(&channel)
    }

    pub fn select(&mut self, channel: Id, cx: &mut Context<Self>) {
        if self.selected != channel && self.workspace.channel(channel).is_some() {
            self.selected = channel;
            if let Some(c) = self.workspace.channels.iter_mut().find(|c| c.id == channel) {
                c.unread = false;
            }
            cx.emit(StoreEvent::Selected);
            cx.notify();
        }
    }

    /// Switches to `channel` and asks its view to scroll to the message at
    /// `index` in its log (logs are append-only, so indices are stable).
    pub fn reveal(&mut self, channel: Id, index: usize, cx: &mut Context<Self>) {
        self.select(channel, cx);
        if self.selected == channel {
            cx.emit(StoreEvent::Reveal { channel, index });
        }
    }

    /// Everything search reads, as a snapshot it can take to another thread.
    /// Only clones `Arc`s, so it is cheap even with very large histories.
    pub fn search_corpus(&self) -> SearchCorpus {
        SearchCorpus {
            channels: self
                .workspace
                .channels
                .iter()
                .map(|c| CorpusChannel {
                    id: c.id,
                    name: c.name.clone(),
                    messages: self
                        .logs
                        .get(&c.id)
                        .map(|log| log.snapshot())
                        .unwrap_or_default(),
                })
                .collect(),
        }
    }

    /// Appends complete messages to a channel. This is where messages from agents
    /// will land once there is a backend: one call per finished message (or batch).
    pub fn append(&mut self, channel: Id, batch: Vec<Message>, cx: &mut Context<Self>) {
        let Some(log) = self.logs.get_mut(&channel) else {
            return;
        };
        let range = log.extend(batch);
        if !range.is_empty() {
            cx.emit(StoreEvent::Appended { channel, range });
        }
    }

    /// Sends a composer message. #main goes through core when it is attached
    /// (the message shows up when core reports it back); everything else is a
    /// local echo until there is somewhere to send to.
    pub fn post_from_human(
        &mut self,
        body: &str,
        reply_to: Option<ReplyRef>,
        cx: &mut Context<Self>,
    ) {
        let body = body.trim();
        if body.is_empty() {
            return;
        }
        if let Some(attached) = &self.core
            && attached.channel == self.selected
        {
            let core = attached.link.core.clone();
            let (author, channel) = (attached.link.human, attached.link.main());
            let reply = reply_to.and_then(|r| core_message_id(r.message));
            let body = body.to_string();
            cx.spawn(async move |this, cx| {
                if let Err(e) = core.post(author, channel, &body, reply).await {
                    this.update(cx, |store, cx| {
                        store.system_note(&format!("Not sent: {e}"), cx)
                    })
                    .ok();
                }
            })
            .detach();
            return;
        }
        let id = Id(self.next_message_id);
        self.next_message_id += 1;
        let sent_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let message = Message {
            id,
            author: Author::Human,
            sent_at,
            body: Arc::from(body),
            reply_to,
        };
        self.append(self.selected, vec![message], cx);
    }

    /// Puts core behind the launch project: its channels replace the sample
    /// ones (each project has its own #main), the picker lists core's
    /// projects, its agents join Agent Status, #main's history loads and its
    /// events flow in from here on.
    pub fn attach_core(&mut self, mut link: CoreLink, cx: &mut Context<Self>) {
        for a in &link.agents {
            self.workspace.agents.push(Agent {
                id: agent_id(a.member),
                name: a.name.clone(),
                brand: a.brand,
                state: AgentState::Sleeping,
                presence: None,
            });
        }
        let channel = self.show_project(&link.open, &link.folder, cx);
        let confirm = link.confirm.take();
        let mut events = link.events.take();
        let core = link.core.clone();
        let main = link.main();
        let task = cx.spawn(async move |this, cx| {
            match core.history(main, 200).await {
                Ok(history) => {
                    let ok = this.update(cx, |store, cx| {
                        for m in history {
                            store.apply_core_event(CoreEvent::Message(m), cx);
                        }
                    });
                    if ok.is_err() {
                        return;
                    }
                }
                Err(e) => eprintln!("chaos-control: could not load #main history: {e}"),
            }
            let Some(events) = events.as_mut() else {
                return;
            };
            while let Some(event) = events.recv().await {
                if this
                    .update(cx, |store, cx| store.apply_core_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.core = Some(Attached {
            link,
            channel,
            seen: HashMap::new(),
            confirm,
            _events: task,
        });
        cx.notify();
    }

    /// Shows `open` as the workspace's project: header, picker and channels.
    /// Every shown channel starts with an empty log (history reloads), and
    /// #main is selected. Returns #main's app id.
    fn show_project(&mut self, open: &OpenProject, folder: &Path, cx: &mut Context<Self>) -> Id {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let shown_path = |root: Option<&str>| {
            root.map(|r| Arc::from(project::display_path(Path::new(r), home.as_deref())))
        };
        let p = &open.project;
        let path = shown_path(p.root_path.as_deref())
            .or_else(|| Some(Arc::from(project::display_path(folder, home.as_deref()))));
        self.workspace.project = model::Project {
            id: Id(0),
            name: p.name.clone(),
            emoji: Arc::from(monogram(&p.name)),
            path,
        };
        self.workspace.projects = open
            .all
            .iter()
            .map(|q| ProjectEntry {
                name: q.name.clone(),
                path: shown_path(q.root_path.as_deref()),
                current: q.id == p.id,
            })
            .collect();
        self.workspace.channels = open
            .channels
            .iter()
            .map(|c| model::Channel {
                id: channel_id(c.id),
                name: c.name.clone(),
                ordinal: Some(c.ordinal),
                linear_issue: None,
                unread: false,
                project_wide: c.id == open.main.id,
            })
            .collect();
        for c in &self.workspace.channels {
            self.logs.insert(c.id, ChannelLog::default());
        }
        let main = channel_id(open.main.id);
        self.selected = main;
        cx.emit(StoreEvent::Selected);
        cx.notify();
        main
    }

    /// The project-identity question to put in the pane, if one is waiting.
    pub fn pending_identity(&self) -> Option<PaneContent> {
        let c = self.core.as_ref()?.confirm.as_ref()?;
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        Some(PaneContent::ProjectIdentity(ProjectQuestion {
            copied_from: match &c.question {
                Question::Changed => None,
                Question::CopiedFrom { root } => Some(Arc::from(project::display_path(
                    Path::new(root),
                    home.as_deref(),
                ))),
            },
            current: Arc::from(c.current.name.as_str()),
            proposed: Arc::from(c.proposed.name.as_str()),
            proposed_id: Arc::from(c.proposed.id.as_str()),
        }))
    }

    /// The user's answer to the identity question. Nothing about the folder's
    /// project changed before this.
    pub fn answer_identity(&mut self, answer: &PaneActionKind, cx: &mut Context<Self>) {
        let Some(attached) = &mut self.core else {
            return;
        };
        let Some(confirm) = attached.confirm.take() else {
            return;
        };
        match answer {
            // Stay, and make the file name what is open so it stops asking.
            PaneActionKind::KeepProject => agents::keep(&attached.link.folder, &confirm.current),
            // Open what the file names; the file already says so.
            PaneActionKind::UseProposedProject => self.switch_to(confirm.proposed, false, cx),
            PaneActionKind::NewProject => {
                let target = Target {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: confirm.proposed.name,
                };
                self.switch_to(target, true, cx);
            }
            _ => attached.confirm = Some(confirm),
        }
    }

    fn switch_to(&mut self, target: Target, write_file: bool, cx: &mut Context<Self>) {
        let Some(attached) = &self.core else {
            return;
        };
        let link = &attached.link;
        let core = link.core.clone();
        let folder = link.folder.clone();
        let members = std::iter::once(link.human)
            .chain(link.agents.iter().map(|a| a.member))
            .collect();
        cx.spawn(async move |this, cx| {
            let opened =
                agents::switch_project(core.clone(), folder, target, write_file, members).await;
            let main = match opened {
                Ok(open) => {
                    let main = open.main.id;
                    let ok = this.update(cx, |store, cx| store.opened(open, cx));
                    if ok.is_err() {
                        return;
                    }
                    main
                }
                Err(e) => {
                    this.update(cx, |store, cx| {
                        store.system_note(&format!("Could not open that project: {e}"), cx)
                    })
                    .ok();
                    return;
                }
            };
            if let Ok(history) = core.history(main, 200).await {
                this.update(cx, |store, cx| {
                    for m in history {
                        store.apply_core_event(CoreEvent::Message(m), cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// A switch finished: show the new project.
    fn opened(&mut self, open: OpenProject, cx: &mut Context<Self>) {
        let Some(mut attached) = self.core.take() else {
            return;
        };
        let folder = attached.link.folder.clone();
        attached.channel = self.show_project(&open, &folder, cx);
        attached.seen.clear();
        attached.link.open = open;
        self.core = Some(attached);
    }

    /// Folds one core event into the app's state.
    pub fn apply_core_event(&mut self, event: CoreEvent, cx: &mut Context<Self>) {
        let Some(attached) = &self.core else {
            return;
        };
        match event {
            CoreEvent::Message(m) => {
                if m.channel != attached.link.main() || attached.seen.contains_key(&m.id) {
                    return;
                }
                let author = match m.author {
                    _ if m.origin == ct::Origin::System => Author::System,
                    None => Author::System,
                    Some(id) if attached.link.agents.iter().any(|a| a.member == id) => {
                        Author::Agent(agent_id(id))
                    }
                    Some(_) => Author::Human,
                };
                let channel = attached.channel;
                let reply_to = m.reply_to.and_then(|r| {
                    let ix = *attached.seen.get(&r)?;
                    self.logs.get(&channel)?.get(ix).map(ReplyRef::to)
                });
                let message = Message {
                    id: Id(CORE_MESSAGE_BASE.saturating_add(m.id.0 as u32)),
                    author,
                    sent_at: m.sent_at / 1000,
                    body: m.body.clone(),
                    reply_to,
                };
                let ix = self.logs.get(&channel).map_or(0, |l| l.len());
                if let Some(attached) = &mut self.core {
                    attached.seen.insert(m.id, ix);
                }
                self.append(channel, vec![message], cx);
            }
            CoreEvent::Agent(status) => {
                let id = agent_id(status.agent);
                if let Some(agent) = self.workspace.agents.iter_mut().find(|a| a.id == id) {
                    agent.state = match status.glow {
                        ct::Glow::Active => AgentState::Active,
                        ct::Glow::Sleeping => AgentState::Sleeping,
                        ct::Glow::Unresponsive => AgentState::Unresponsive,
                    };
                    agent.presence = status.presence.clone();
                    cx.notify();
                }
            }
            // The model has no reactions yet.
            CoreEvent::Reaction { .. } => {}
        }
    }

    /// A local notice in the selected channel (e.g. a post core refused).
    fn system_note(&mut self, body: &str, cx: &mut Context<Self>) {
        let id = Id(self.next_message_id);
        self.next_message_id += 1;
        let sent_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let message = Message {
            id,
            author: Author::System,
            sent_at,
            body: Arc::from(body),
            reply_to: None,
        };
        self.append(self.selected, vec![message], cx);
    }

    /// The sample conversation in #main (see `model::sample_history`).
    pub fn load_sample_history(&mut self, cx: &mut Context<Self>) {
        let (channel, messages) = model::sample_history();
        self.append(channel, messages, cx);
    }

    pub fn load_demo_history(&mut self, count: usize, cx: &mut Context<Self>) {
        let main = self.workspace.channels[0].id;
        self.append(main, model::demo_history(count).collect(), cx);
    }
}

fn channel_id(channel: ct::ChannelId) -> Id {
    Id(CORE_CHANNEL_BASE.saturating_add(channel.0 as u32))
}

/// The picker's badge for a project without an emoji: its first letter.
fn monogram(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "#".to_string(), |c| c.to_uppercase().collect())
}

fn agent_id(member: ct::MemberId) -> Id {
    Id(CORE_AGENT_BASE.saturating_add(member.0 as u32))
}

fn core_message_id(id: Id) -> Option<ct::MessageId> {
    id.0.checked_sub(CORE_MESSAGE_BASE)
        .map(|n| ct::MessageId(i64::from(n)))
}
