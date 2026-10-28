//! The app's single source of truth for chat state.
//!
//! Views hold an `Entity<ChatStore>` and subscribe to its events instead of
//! sharing data by cloning it around. Each event says exactly what changed, so a
//! view only re-renders (or re-measures list rows) for the part it shows.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::Arc,
};

use chaos_core::{engine::CoreEvent, types as ct};
use gpui::{Context, EventEmitter, Task};
use model::{
    Agent, AgentState, Author, ChannelLog, CorpusChannel, Id, Message, PaneActionKind, PaneContent,
    ProjectEntry, ProjectQuestion, ProjectQuestionKind, ReplyRef, SearchCorpus, Stance, Workspace,
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
    /// Read receipts moved: these rows of `channel` gained or lost readers
    /// (their heights may change).
    Receipts { channel: Id, rows: Vec<usize> },
}

/// Someone who has read up to a message (Slack/iMessage-style read marks).
#[derive(Clone, Debug)]
pub struct Reader {
    pub author: Author,
    pub name: Arc<str>,
    /// When they got this far, seconds since the Unix epoch.
    pub read_at: u64,
}

pub struct ChatStore {
    workspace: Workspace,
    logs: HashMap<Id, ChannelLog>,
    selected: Id,
    next_message_id: u32,
    core: Option<Attached>,
}

/// How [`ChatStore::switch_to`] opens the project.
enum Switch {
    /// Open this project (writing the folder's file if `true`).
    Open(Target, bool),
    /// Adopt the unclaimed pre-project history as this folder's project.
    Adopt(Target),
}

/// Core behind #main (see `agents.rs`).
struct Attached {
    link: CoreLink,
    /// The app channel core's main channel shows in.
    channel: Id,
    /// Core message id -> index in that channel's log (dedups history vs events).
    seen: HashMap<ct::MessageId, usize>,
    /// Core seq -> index in that channel's log, for placing read receipts.
    by_seq: BTreeMap<ct::Seq, usize>,
    /// The newest read receipt per member in #main (never the local human).
    receipts: HashMap<ct::MemberId, ct::Receipt>,
    /// Member names for receipts from people who aren't linked agents.
    names: HashMap<ct::MemberId, Arc<str>>,
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
            self.mark_main_read(cx);
        }
    }

    /// Who has read up to (and no further than) the message at `index` of
    /// `channel`, oldest read first. Only core channels have receipts.
    pub fn readers(&self, channel: Id, index: usize) -> Vec<Reader> {
        let Some(attached) = self.core.as_ref().filter(|a| a.channel == channel) else {
            return Vec::new();
        };
        let mut out: Vec<Reader> = attached
            .receipts
            .values()
            .filter(|r| row_at(&attached.by_seq, r.read_through) == Some(index))
            .map(|r| {
                let linked = attached.link.agents.iter().find(|a| a.member == r.member);
                Reader {
                    author: match linked {
                        Some(a) => Author::Agent(agent_id(a.member)),
                        None => Author::Human,
                    },
                    name: linked
                        .map(|a| a.name.clone())
                        .or_else(|| attached.names.get(&r.member).cloned())
                        .unwrap_or_else(|| Arc::from("Someone")),
                    read_at: r.read_at / 1000,
                }
            })
            .collect();
        out.sort_by(|a, b| a.read_at.cmp(&b.read_at).then(a.name.cmp(&b.name)));
        out
    }

    /// The human at this keyboard has seen #main up to its newest message
    /// while it is the selected channel. Core ignores a cursor that would not
    /// move, so repeating this is cheap.
    fn mark_main_read(&mut self, cx: &mut Context<Self>) {
        let Some(attached) = &self.core else {
            return;
        };
        if attached.channel != self.selected || attached.by_seq.is_empty() {
            return;
        }
        let core = attached.link.core.clone();
        let (human, main) = (attached.link.human, attached.link.main());
        cx.spawn(async move |_, _| {
            if let Err(e) = core.mark_read(human, main, None).await {
                eprintln!("chaos-control: could not record a read receipt: {e}");
            }
        })
        .detach();
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
            stance: None,
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
            if let Ok(members) = core.members(main).await {
                let ok = this.update(cx, |store, _| {
                    if let Some(a) = &mut store.core {
                        a.names = members.into_iter().map(|m| (m.id, m.name)).collect();
                    }
                });
                if ok.is_err() {
                    return;
                }
            }
            if let Ok(receipts) = core.receipts(main).await {
                let ok = this.update(cx, |store, cx| {
                    for r in receipts {
                        store.apply_core_event(CoreEvent::Read(r), cx);
                    }
                    store.mark_main_read(cx);
                });
                if ok.is_err() {
                    return;
                }
            }
            let Some(events) = events.as_mut() else {
                return;
            };
            while let Some(event) = events.recv().await {
                let message = matches!(event, CoreEvent::Message(_));
                if this
                    .update(cx, |store, cx| {
                        store.apply_core_event(event, cx);
                        if message {
                            store.mark_main_read(cx);
                        }
                    })
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
            by_seq: BTreeMap::new(),
            receipts: HashMap::new(),
            names: HashMap::new(),
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
            kind: match &c.question {
                Question::Changed => ProjectQuestionKind::Changed,
                Question::CopiedFrom { root } => ProjectQuestionKind::CopiedFrom(Arc::from(
                    project::display_path(Path::new(root), home.as_deref()),
                )),
                Question::Imported => ProjectQuestionKind::Imported,
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
            // For the import question this is "Start fresh".
            PaneActionKind::KeepProject => agents::keep(&attached.link.folder, &confirm.current),
            // Open what the file names; the file already says so. A copy's
            // separate project was only a placeholder: it goes if still empty.
            PaneActionKind::UseProposedProject => {
                let forget = match confirm.question {
                    Question::CopiedFrom { .. } => Some(confirm.current.id.clone()),
                    _ => None,
                };
                self.switch_to(Switch::Open(confirm.proposed, false), forget, cx)
            }
            PaneActionKind::NewProject => {
                let target = Target {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: confirm.proposed.name,
                };
                self.switch_to(Switch::Open(target, true), None, cx);
            }
            // Give this folder the pre-project history; the fresh project
            // opened meanwhile goes if nothing was said in it.
            PaneActionKind::AdoptImported if confirm.question == Question::Imported => {
                let forget = Some(confirm.current.id.clone());
                self.switch_to(Switch::Adopt(confirm.proposed), forget, cx)
            }
            _ => attached.confirm = Some(confirm),
        }
    }

    /// Opens another project in this folder, then deletes `forget` (the
    /// project that was open) if the switch left it empty and folderless.
    fn switch_to(&mut self, how: Switch, forget: Option<String>, cx: &mut Context<Self>) {
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
            let opened = match how {
                Switch::Open(target, write_file) => {
                    agents::switch_project(core.clone(), folder, target, write_file, members).await
                }
                Switch::Adopt(target) => {
                    agents::adopt_import(core.clone(), folder, target, members).await
                }
            };
            let main = match opened {
                Ok(open) => {
                    let main = open.main.id;
                    if let Some(id) = forget.filter(|id| *id != open.project.id.to_string()) {
                        agents::forget_placeholder(&core, &id).await;
                    }
                    // The picker re-reads projects so a forgotten one is gone.
                    let open = match core.projects().await {
                        Ok(all) => OpenProject { all, ..open },
                        Err(_) => open,
                    };
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
                    store.mark_main_read(cx);
                })
                .ok();
            }
            if let Ok(receipts) = core.receipts(main).await {
                this.update(cx, |store, cx| {
                    for r in receipts {
                        store.apply_core_event(CoreEvent::Read(r), cx);
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
        attached.by_seq.clear();
        attached.receipts.clear();
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
                    stance: match m.origin {
                        ct::Origin::Refusal => Some(Stance::Refusal),
                        ct::Origin::Dissent => Some(Stance::Dissent),
                        _ => None,
                    },
                };
                let ix = self.logs.get(&channel).map_or(0, |l| l.len());
                // A receipt already past this message moves down onto it.
                let moved: Vec<usize> = attached
                    .receipts
                    .values()
                    .filter(|r| r.read_through >= m.seq)
                    .filter_map(|r| row_at(&attached.by_seq, r.read_through))
                    .collect();
                if let Some(attached) = &mut self.core {
                    attached.seen.insert(m.id, ix);
                    attached.by_seq.insert(m.seq, ix);
                }
                self.append(channel, vec![message], cx);
                if !moved.is_empty() {
                    let rows = moved.into_iter().chain([ix]).collect();
                    cx.emit(StoreEvent::Receipts { channel, rows });
                }
            }
            CoreEvent::Read(r) => {
                if r.channel != attached.link.main() || r.member == attached.link.human {
                    return;
                }
                let channel = attached.channel;
                let old = attached
                    .receipts
                    .get(&r.member)
                    .and_then(|k| row_at(&attached.by_seq, k.read_through));
                let new = row_at(&attached.by_seq, r.read_through);
                if let Some(attached) = &mut self.core {
                    attached.receipts.insert(r.member, r);
                }
                let rows: Vec<usize> = old.into_iter().chain(new).collect();
                if !rows.is_empty() {
                    cx.emit(StoreEvent::Receipts { channel, rows });
                }
                cx.notify();
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
            stance: None,
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

/// The log row for the newest message at or before `seq`, if any is loaded.
fn row_at(by_seq: &BTreeMap<ct::Seq, usize>, seq: ct::Seq) -> Option<usize> {
    by_seq.range(..=seq).next_back().map(|(_, ix)| *ix)
}
