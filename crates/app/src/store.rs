//! The app's single source of truth for chat state.
//!
//! Views hold an `Entity<ChatStore>` and subscribe to its events instead of
//! sharing data by cloning it around. Each event says exactly what changed, so a
//! view only re-renders (or re-measures list rows) for the part it shows.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use chaos_core::{
    engine::CoreEvent,
    harness::{self, Catalog, HarnessModels, ModelInfo, ModelSource},
    model_cache::{self, CachedModels, Freshness},
    permission::{LogOutcome, PermissionId, PermissionRequest, PermissionState},
    types as ct,
};
use gpui::{Context, EventEmitter, Task};
use model::{
    Agent, AgentState, Author, ChannelLog, CorpusChannel, Id, Message, PERMISSION_LOG_LEN,
    PaneActionKind, PaneContent, PermissionLog, PermissionLogEntry, PermissionOutcome,
    PermissionPrompt, ProjectEntry, ProjectQuestion, ProjectQuestionKind, ReplyRef, SearchCorpus,
    Stance, Workspace,
};

use crate::{
    agents::{self, CoreLink, DormantMember, LinkedAgent, OpenProject},
    agents_file,
    project::{self, Confirm, Question, Target},
};

/// Whether an agent's permission request opens the Approve / Deny pane.
/// Muted (Hxmbl, 2026-10-31): requests are answered at once (reads allowed,
/// everything else declined) and listed in the right sidebar's permission
/// log instead. The pane path stays wired for when human approvals return.
const PERMISSION_PANE: bool = false;

/// How long after a failed probe (signed out, timed out) a harness is left
/// alone before the picker opening asks it again.
const PROBE_RETRY: Duration = Duration::from_secs(5 * 60);

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
    /// An agent asked for permission: show it in the pane (Approve / Deny).
    PermissionAsked(PaneContent),
    /// A permission request was answered (here or by someone else): close its
    /// pane if it is still showing.
    PermissionAnswered { request: i64 },
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
    /// #main's permission requests, newest first (the right sidebar's log).
    permissions: PermissionLog,
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
    /// Each usable harness's models: its cached list (`model_cache`), else
    /// its preset list until it reports its own.
    models: HashMap<&'static str, HarnessModels>,
    /// Harnesses being asked for their models right now (in the background,
    /// on core's runtime; never while the user waits).
    discovering: HashSet<&'static str>,
    /// Harnesses whose cached list is missing, stale or from another
    /// version: probed in the background.
    stale: HashSet<&'static str>,
    /// When a harness's last probe failed, so a signed-out harness isn't
    /// started again every time the picker opens.
    failed_at: HashMap<&'static str, Instant>,
    /// Every agent member core knows, current or removed: their messages
    /// read as an agent's ("Previous agent" once they're gone).
    agent_ids: HashSet<ct::MemberId>,
    _events: Task<()>,
}

/// One harness in the model picker's filter row.
#[derive(Clone, Debug)]
pub struct HarnessChip {
    pub id: &'static str,
    pub name: &'static str,
    pub brand: model::Brand,
    /// Asking the harness for its models right now.
    pub checking: bool,
    /// Its list is the preset guess, not the harness's own answer.
    pub preset_only: bool,
    /// Why its own list is missing ("signed out"), if a discovery failed.
    pub note: Option<Arc<str>>,
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
            permissions: PermissionLog::default(),
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// #main's permission requests, newest first (reads included).
    pub fn permission_log(&self) -> &PermissionLog {
        &self.permissions
    }

    /// Replaces the permission log with what core has stored.
    fn reset_permission_log(&mut self, rows: Vec<PermissionRequest>, cx: &mut Context<Self>) {
        let entries = rows.iter().map(|r| self.log_entry(r)).collect();
        self.permissions.reset(entries);
        cx.notify();
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
        // Agent Status is the project's members now, not the sample cast
        // (and the sample mentions name sample agents).
        self.workspace.agents = link.agents.iter().map(status_agent).collect();
        self.workspace.mentions.clear();
        // Each harness starts on its cached list when there is one (shown at
        // once, even if stale), else its preset list. Missing or stale lists
        // are probed in the background right after this.
        let now = model_cache::now_secs();
        let mut stale = HashSet::new();
        let models: HashMap<&'static str, HarnessModels> = link
            .harnesses
            .iter()
            .map(|h| {
                let cached = link.cached_models.get(h.preset.id);
                if cached.is_none_or(|c| c.freshness(&h.version(), now) != Freshness::Fresh) {
                    stale.insert(h.preset.id);
                }
                let entry = match cached {
                    Some(c) if !c.models.is_empty() => HarnessModels {
                        harness: h.preset,
                        source: ModelSource::Live,
                        models: with_default(h.preset, c.models()),
                        note: None,
                    },
                    _ => HarnessModels {
                        harness: h.preset,
                        source: ModelSource::Preset,
                        models: harness::preset_models(h.preset),
                        note: None,
                    },
                };
                (h.preset.id, entry)
            })
            .collect();
        let channel = self.show_project(&link.open, &link.folder, cx);
        let confirm = link.confirm.take();
        let mut events = link.events.take();
        let core = link.core.clone();
        let main = link.main();
        let task = cx.spawn(async move |this, cx| {
            // Who was ever an agent, before the history that needs it.
            if let Ok(ids) = core.agent_members().await {
                let ok = this.update(cx, |store, _| {
                    if let Some(a) = &mut store.core {
                        a.agent_ids.extend(ids);
                    }
                });
                if ok.is_err() {
                    return;
                }
            }
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
            // The permission log, as core stored it (requests asked while
            // the app was closed included).
            if let Ok(log) = core.permission_log(main, PERMISSION_LOG_LEN).await {
                let ok = this.update(cx, |store, cx| store.reset_permission_log(log, cx));
                if ok.is_err() {
                    return;
                }
            }
            // Requests asked while the app was closed are still pending
            // (they only reach the pane when it isn't muted).
            if PERMISSION_PANE && let Ok(pending) = core.pending_permissions(main).await {
                let ok = this.update(cx, |store, cx| {
                    for r in pending {
                        store.apply_core_event(CoreEvent::Permission(r), cx);
                    }
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
            models,
            discovering: HashSet::new(),
            stale,
            failed_at: HashMap::new(),
            agent_ids: HashSet::new(),
            _events: task,
        });
        // Probe the harnesses without a fresh cached list, in the background.
        self.refresh_models(cx);
        cx.notify();
    }

    // ---------- members (models through harnesses) ----------

    /// Whether core runs (so members can be added and removed).
    pub fn has_core(&self) -> bool {
        self.core.is_some()
    }

    /// Every usable harness's models, grouped by model (see
    /// `harness::Catalog`): the model picker's rows.
    pub fn catalog(&self) -> Catalog {
        let Some(attached) = &self.core else {
            return Catalog::default();
        };
        let lists: Vec<HarnessModels> = attached
            .link
            .harnesses
            .iter()
            .filter_map(|h| attached.models.get(h.preset.id).cloned())
            .collect();
        Catalog::build(&lists)
    }

    /// The picker's harness filter, in preset order.
    pub fn harness_chips(&self) -> Vec<HarnessChip> {
        let Some(attached) = &self.core else {
            return Vec::new();
        };
        attached
            .link
            .harnesses
            .iter()
            .map(|h| {
                let m = attached.models.get(h.preset.id);
                HarnessChip {
                    id: h.preset.id,
                    name: h.preset.name,
                    brand: h.brand(),
                    checking: attached.discovering.contains(h.preset.id),
                    preset_only: m.is_none_or(|m| m.source == ModelSource::Preset),
                    note: m.and_then(|m| m.note.clone()),
                }
            })
            .collect()
    }

    /// Roster members whose harness isn't installed (the picker lists them
    /// greyed, "not installed").
    pub fn dormant(&self) -> &[DormantMember] {
        match &self.core {
            Some(a) => &a.link.dormant,
            None => &[],
        }
    }

    /// Whether the member running `model` on `harness` is already here.
    pub fn has_member(&self, harness: &str, model: Option<&str>) -> bool {
        self.core.as_ref().is_some_and(|a| {
            a.link
                .agents
                .iter()
                .any(|x| x.harness_id.as_deref() == Some(harness) && x.model.as_deref() == model)
        })
    }

    /// Probes, in the background, every harness whose cached model list is
    /// missing, stale or from another version (see `model_cache`). Called
    /// when core starts and when the model picker opens; it only schedules
    /// work and returns at once. Each probe spawns its harness once, on
    /// core's own tokio runtime (`Core::probe_models`), and its answer is
    /// written to the cache and lands in the picker. A harness whose probe
    /// failed is left alone for `PROBE_RETRY`.
    pub fn refresh_models(&mut self, cx: &mut Context<Self>) {
        let Some(attached) = &mut self.core else {
            return;
        };
        let core = attached.link.core.clone();
        let folder = attached.link.folder.clone();
        let cache_dir = attached.link.model_cache_dir.clone();
        let todo: Vec<agents::Harness> = attached
            .link
            .harnesses
            .iter()
            .filter(|h| attached.stale.contains(h.preset.id))
            .filter(|h| !attached.discovering.contains(h.preset.id))
            .filter(|h| {
                attached
                    .failed_at
                    .get(h.preset.id)
                    .is_none_or(|at| at.elapsed() >= PROBE_RETRY)
            })
            .cloned()
            .collect();
        for h in todo {
            let id = h.preset.id;
            attached.discovering.insert(id);
            let (core, folder, cache_dir) = (core.clone(), folder.clone(), cache_dir.clone());
            cx.spawn(async move |this, cx| {
                let version = h.version();
                let found = core
                    .probe_models(id, &version, h.adapter(None), folder, cache_dir)
                    .await;
                this.update(cx, |store, cx| {
                    if let Some(a) = &mut store.core {
                        a.discovering.remove(id);
                        match &found {
                            Ok(_) => {
                                a.stale.remove(id);
                                a.failed_at.remove(id);
                            }
                            Err(_) => {
                                a.failed_at.insert(id, Instant::now());
                            }
                        }
                    }
                    match found {
                        Ok(cached) => store.set_models(id, cached.models(), cx),
                        Err(e) => store.note_models(id, Some(short_reason(&e)), cx),
                    }
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    /// A harness's own model list arrived.
    fn set_models(&mut self, id: &str, list: Vec<ModelInfo>, cx: &mut Context<Self>) {
        let Some(attached) = &mut self.core else {
            return;
        };
        let Some(entry) = attached.models.values_mut().find(|m| m.harness.id == id) else {
            return;
        };
        entry.models = with_default(entry.harness, list);
        entry.source = ModelSource::Live;
        entry.note = None;
        cx.notify();
    }

    /// Discovery for `id` ended without a list (`why` when it failed).
    fn note_models(&mut self, id: &str, why: Option<Arc<str>>, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .core
            .as_mut()
            .and_then(|a| a.models.values_mut().find(|m| m.harness.id == id))
        {
            entry.note = why;
            cx.notify();
        }
    }

    /// Adds the member running `model` on `harness` to this project. It
    /// shows in Agent Status once core has it (sleeping until pinged).
    pub fn add_member(
        &mut self,
        harness: &str,
        model: Option<Arc<str>>,
        name: Arc<str>,
        cx: &mut Context<Self>,
    ) {
        let Some(attached) = &self.core else {
            return;
        };
        if self.has_member(harness, model.as_deref()) {
            return;
        }
        let Some(h) = attached.link.harness(harness).cloned() else {
            return;
        };
        let core = attached.link.core.clone();
        let main = attached.link.main();
        let path = attached.link.agents_path.clone();
        let entry = agents_file::Entry::new(harness, model.as_deref(), &name);
        cx.spawn(async move |this, cx| {
            let added = agents::add_member(core, h, model, name, main).await;
            this.update(cx, |store, cx| match added {
                Ok(linked) => {
                    // The roster is agents.json's `members` (see agents_file).
                    agents_file::in_background(path, move |p| agents_file::add_member(p, &entry));
                    store.linked(linked, cx)
                }
                Err(e) => store.system_note(&format!("Could not add that model: {e}"), cx),
            })
            .ok();
        })
        .detach();
    }

    fn linked(&mut self, linked: LinkedAgent, cx: &mut Context<Self>) {
        let Some(attached) = &mut self.core else {
            return;
        };
        if attached
            .link
            .agents
            .iter()
            .any(|a| a.member == linked.member)
        {
            return;
        }
        attached.names.insert(linked.member, linked.name.clone());
        attached.agent_ids.insert(linked.member);
        self.workspace.agents.push(status_agent(&linked));
        attached.link.agents.push(linked);
        cx.notify();
    }

    /// Takes a roster member out of this project (Agent Status's remove).
    /// Its messages stay in the chat.
    pub fn remove_agent(&mut self, id: Id, cx: &mut Context<Self>) {
        let Some(attached) = &mut self.core else {
            return;
        };
        let Some(ix) = attached
            .link
            .agents
            .iter()
            .position(|a| agent_id(a.member) == id && a.removable)
        else {
            return;
        };
        let removed = attached.link.agents.remove(ix);
        let member = removed.member;
        // Its messages stay, and read as a previous agent's.
        attached.agent_ids.insert(member);
        if let Some(harness) = removed.harness_id.clone() {
            let model = removed.model.clone();
            agents_file::in_background(attached.link.agents_path.clone(), move |p| {
                agents_file::remove_member(p, &harness, model.as_deref())
            });
        }
        self.workspace.agents.retain(|a| a.id != id);
        let core = attached.link.core.clone();
        let project = attached.link.open.project.id.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            if let Err(e) = core.remove_member(&project, member).await {
                this.update(cx, |store, cx| {
                    store.system_note(&format!("Could not remove that member: {e}"), cx)
                })
                .ok();
            }
        })
        .detach();
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
            if let Ok(log) = core.permission_log(main, PERMISSION_LOG_LEN).await {
                this.update(cx, |store, cx| store.reset_permission_log(log, cx))
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
        // Each project's #main has its own log; it reloads after the switch.
        self.permissions = PermissionLog::default();
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
                    Some(id)
                        if attached.link.agents.iter().any(|a| a.member == id)
                            || attached.agent_ids.contains(&id) =>
                    {
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
            CoreEvent::Permission(r) => {
                if r.channel != attached.link.main() {
                    return;
                }
                let entry = self.log_entry(&r);
                self.permissions.upsert(entry);
                cx.notify();
                // Muted: no request opens the pane (see `PERMISSION_PANE`).
                if !PERMISSION_PANE {
                    return;
                }
                if r.state == PermissionState::Pending {
                    let prompt = self.permission_prompt(&r);
                    cx.emit(StoreEvent::PermissionAsked(PaneContent::Permission(prompt)));
                } else {
                    cx.emit(StoreEvent::PermissionAnswered { request: r.id.0 });
                }
            }
            // The model has no reactions yet.
            CoreEvent::Reaction { .. } => {}
            CoreEvent::Models { harness, models } => {
                // A live session reported its harness's models: show them and
                // keep them in the on-disk cache (off the UI thread).
                let found = attached.link.harness(&harness).map(|h| {
                    (
                        h.preset.id,
                        h.version(),
                        attached.link.model_cache_dir.clone(),
                    )
                });
                if let Some((id, version, dir)) = found
                    && !models.is_empty()
                {
                    let cached = CachedModels::new(id, &version, model_cache::now_secs(), &models);
                    std::thread::spawn(move || {
                        if let Err(e) = model_cache::save(&dir, &cached) {
                            eprintln!("chaos-control: could not cache the {id} models: {e}");
                        }
                    });
                    if let Some(a) = &mut self.core {
                        a.stale.remove(id);
                    }
                    self.set_models(id, models, cx);
                }
            }
        }
    }

    /// A core member's display name, if the app knows it.
    fn member_name(&self, id: ct::MemberId) -> Option<Arc<str>> {
        let attached = self.core.as_ref()?;
        attached
            .link
            .agents
            .iter()
            .find(|x| x.member == id)
            .map(|x| x.name.clone())
            .or_else(|| attached.names.get(&id).cloned())
    }

    /// Who a request was routed to, by name ("Hx").
    fn asked_names(&self, r: &PermissionRequest) -> Arc<str> {
        let asked = r
            .asked_of
            .iter()
            .map(|m| {
                self.member_name(*m)
                    .map_or_else(|| "someone".to_string(), |n| n.to_string())
            })
            .collect::<Vec<_>>()
            .join(", ");
        Arc::from(asked)
    }

    /// The pane's view of a pending permission request.
    fn permission_prompt(&self, r: &PermissionRequest) -> PermissionPrompt {
        PermissionPrompt {
            request: r.id.0,
            agent: self
                .member_name(r.agent)
                .unwrap_or_else(|| Arc::from("An agent")),
            action: r.title.clone(),
            reason: r.reason.clone(),
            detail: r.detail.clone(),
            asked: self.asked_names(r),
            logged: None,
        }
    }

    /// The permission log's row for a stored request. A member who has left
    /// reads as "Previous agent", with no mark.
    fn log_entry(&self, r: &PermissionRequest) -> PermissionLogEntry {
        let agent = self.workspace.agent(agent_id(r.agent));
        PermissionLogEntry {
            request: r.id.0,
            agent: agent.map(|a| a.id),
            brand: agent.map(|a| a.brand),
            agent_name: agent.map_or_else(
                || {
                    self.member_name(r.agent)
                        .unwrap_or_else(|| Arc::from(model::PREVIOUS_AGENT))
                },
                |a| a.name.clone(),
            ),
            action: r.title.clone(),
            outcome: match r.outcome() {
                LogOutcome::AutoAllowed => PermissionOutcome::AutoAllowed,
                LogOutcome::Declined => PermissionOutcome::Declined,
                LogOutcome::Approved => PermissionOutcome::Approved,
            },
            at: r.created_at / 1000,
            read_only: r.read_only,
            reason: r.reason.clone(),
            detail: r.detail.clone(),
            asked: self.asked_names(r),
        }
    }

    /// The local human's answer to a permission request. Core posts it and
    /// wakes the agent; a refusal (already answered) shows as a local note.
    pub fn answer_permission(&mut self, request: i64, approve: bool, cx: &mut Context<Self>) {
        let Some(attached) = &self.core else {
            return;
        };
        let core = attached.link.core.clone();
        let human = attached.link.human;
        cx.spawn(async move |this, cx| {
            if let Err(e) = core
                .answer_permission(PermissionId(request), human, approve, None)
                .await
            {
                this.update(cx, |store, cx| {
                    store.system_note(&format!("Couldn't answer request #{request}: {e}"), cx)
                })
                .ok();
            }
        })
        .detach();
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

/// How a linked agent shows in Agent Status (sleeping until core says).
fn status_agent(a: &LinkedAgent) -> Agent {
    Agent {
        id: agent_id(a.member),
        name: a.name.clone(),
        brand: a.brand,
        state: AgentState::Sleeping,
        presence: None,
        harness: a.harness.clone(),
        removable: a.removable,
    }
}

/// A harness's model list with "the harness default" first: it is always a
/// valid choice, whatever the harness listed.
fn with_default(preset: &'static harness::HarnessPreset, list: Vec<ModelInfo>) -> Vec<ModelInfo> {
    let mut models = vec![ModelInfo::default_for(preset)];
    models.extend(list.into_iter().filter(|m| m.id.is_some()));
    models
}

/// A discovery error, short enough for the picker ("signed out").
fn short_reason(e: &str) -> Arc<str> {
    let lower = e.to_lowercase();
    if ["auth", "login", "log in", "sign in"]
        .iter()
        .any(|k| lower.contains(k))
    {
        return Arc::from("signed out");
    }
    Arc::from(e.chars().take(80).collect::<String>())
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
