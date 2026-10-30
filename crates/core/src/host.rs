//! The in-process runtime: one actor task owns the [`Engine`], the agent
//! sessions and the chat MCP server, behind the cloneable [`Core`] handle.
//!
//! The handle is the API boundary that could later move behind a daemon: every
//! call is a message to the actor, every answer comes back on a channel, and UI
//! updates arrive as [`CoreEvent`]s.

use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc};

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    adapter::*,
    engine::{
        AgentStatus, CoreEvent, Dispatch, Effect, Engine, PostError, PostRequest, StanceRequest,
    },
    mcp::{self, Scope, Tokens},
    permission::{
        AnswerError, PermissionGate, PermissionId, PermissionRequest, PermissionVerdict, RunsIn,
    },
    policy::Policy,
    store::Store,
    types::*,
};

// ---------- clock ----------

/// The runtime's only source of time. Tests use [`ManualClock`].
pub trait Clock: Send + Sync {
    fn now(&self) -> Millis;
    fn sleep_until(&self, deadline: Millis) -> BoxFuture<'static, ()>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Millis {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as Millis)
            .unwrap_or_default()
    }

    fn sleep_until(&self, deadline: Millis) -> BoxFuture<'static, ()> {
        let wait = deadline.saturating_sub(self.now());
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(wait)))
    }
}

/// A clock that only moves when told to.
pub struct ManualClock {
    now: watch::Sender<Millis>,
}

impl ManualClock {
    pub fn new(start: Millis) -> Arc<Self> {
        Arc::new(ManualClock {
            now: watch::channel(start).0,
        })
    }

    pub fn advance(&self, ms: Millis) {
        self.now.send_modify(|t| *t += ms);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Millis {
        *self.now.borrow()
    }

    fn sleep_until(&self, deadline: Millis) -> BoxFuture<'static, ()> {
        let mut rx = self.now.subscribe();
        Box::pin(async move {
            let _ = rx.wait_for(|t| *t >= deadline).await;
        })
    }
}

// ---------- config ----------

pub struct HostConfig {
    /// SQLite file; `None` for an in-memory database.
    pub db: Option<PathBuf>,
    pub policy: Policy,
    /// Where the chat MCP server listens (port 0 = ephemeral).
    pub mcp_bind: SocketAddr,
    /// How agents can start the stdio bridge, if this process can be one.
    pub bridge: Option<BridgeCommand>,
    /// Working directory for agents that don't set their own.
    pub default_cwd: PathBuf,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig {
            db: None,
            policy: Policy::default(),
            mcp_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            bridge: None,
            default_cwd: std::env::temp_dir(),
        }
    }
}

pub struct AgentSpec {
    pub handle: String,
    pub name: String,
    pub adapter: Arc<dyn AgentAdapter>,
    pub cwd: Option<PathBuf>,
    /// Where the agent runs: on the host its permission requests go to the
    /// humans; in its own VM they are approved (see `permission`).
    pub runs_in: RunsIn,
}

// ---------- handle ----------

type EngineFn = Box<dyn FnOnce(&mut Engine, Millis) + Send>;

enum Cmd {
    Engine(EngineFn),
    AddAgent {
        spec: AgentSpec,
        channel: ChannelId,
        reply: oneshot::Sender<Result<Member, String>>,
    },
    Opened {
        agent: MemberId,
        channel: ChannelId,
        epoch: u64,
        result: Result<Arc<dyn AgentSession>, AgentError>,
    },
    Event {
        agent: MemberId,
        channel: ChannelId,
        epoch: u64,
        event: AgentEvent,
    },
    DeliverFailed {
        agent: MemberId,
        channel: ChannelId,
        epoch: u64,
        batch: BatchId,
        error: AgentError,
    },
    Interrupt {
        agent: MemberId,
        channel: ChannelId,
    },
    RemoveMember {
        project: ProjectId,
        member: MemberId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Shutdown(oneshot::Sender<()>),
}

/// A model member to add (or re-register at startup): which model, through
/// which harness, and the adapter that harness gets for it.
pub struct MemberSpec {
    pub harness: String,
    /// The harness's model id; `None` runs its default model.
    pub model: Option<String>,
    pub model_name: String,
    pub adapter: Arc<dyn AgentAdapter>,
    pub cwd: Option<PathBuf>,
    pub runs_in: RunsIn,
}

/// Cheap, cloneable handle to the runtime.
#[derive(Clone)]
pub struct Core {
    tx: mpsc::UnboundedSender<Cmd>,
    mcp_url: Arc<str>,
    tokens: Arc<Tokens>,
}

const GONE: &str = "chaos-control core has stopped";

impl Core {
    /// Starts the runtime on the current tokio runtime. Returns the handle and
    /// the stream of UI events.
    pub async fn start(
        config: HostConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<(Core, mpsc::UnboundedReceiver<CoreEvent>), String> {
        let store = match &config.db {
            Some(path) => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                Store::open(path)
            }
            None => Store::open_in_memory(),
        }
        .map_err(|e| format!("open database: {e}"))?;
        let engine = Engine::new(store, config.policy.clone(), clock.now())
            .map_err(|e| format!("recover: {e}"))?;
        let (tx, rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let tokens = Arc::new(Tokens::default());
        let caller_tx = tx.clone();
        let caller: mcp::ToolCaller = Arc::new(move |scope: Scope, name: String, args: Value| {
            let tx = caller_tx.clone();
            Box::pin(async move {
                let (reply, wait) = oneshot::channel();
                let f: EngineFn = Box::new(move |engine, now| {
                    let r = mcp::call_tool(engine, now, scope, &name, &args).map_err(|e| e.0);
                    let _ = reply.send(r);
                });
                tx.send(Cmd::Engine(f)).map_err(|_| GONE.to_string())?;
                wait.await.map_err(|_| GONE.to_string())?
            })
        });
        let mcp_url = mcp::serve(config.mcp_bind, tokens.clone(), caller)
            .await
            .map_err(|e| format!("start chat MCP server: {e}"))?;
        let actor = Actor {
            engine,
            clock,
            tokens: tokens.clone(),
            mcp_url: mcp_url.clone(),
            bridge: config.bridge,
            default_cwd: config.default_cwd,
            agents: HashMap::new(),
            slots: HashMap::new(),
            next_epoch: 1,
            tx: tx.clone(),
            events: events_tx,
            alive: true,
        };
        tokio::spawn(actor.run(rx));
        Ok((
            Core {
                tx,
                mcp_url: Arc::from(mcp_url),
                tokens,
            },
            events_rx,
        ))
    }

    pub fn mcp_url(&self) -> &str {
        &self.mcp_url
    }

    /// Live (unrevoked) chat tokens. For tests and diagnostics.
    pub fn live_tokens(&self) -> usize {
        self.tokens.len()
    }

    /// Runs `f` on the engine inside the actor and returns its result.
    pub async fn with_engine<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Engine, Millis) -> R + Send + 'static,
    ) -> Result<R, String> {
        let (reply, wait) = oneshot::channel();
        let f: EngineFn = Box::new(move |engine, now| {
            let _ = reply.send(f(engine, now));
        });
        self.tx.send(Cmd::Engine(f)).map_err(|_| GONE.to_string())?;
        wait.await.map_err(|_| GONE.to_string())
    }

    /// Creates the project `id` or refreshes its display name and the folder
    /// it was last seen in (`root`). Deciding *which* id a folder gets, and
    /// asking the user when that changes, is the caller's job; this grants
    /// no one access to anything.
    pub async fn ensure_project(
        &self,
        id: ProjectId,
        name: &str,
        root: Option<&str>,
    ) -> Result<Project, String> {
        let (name, root) = (name.to_string(), root.map(str::to_string));
        self.with_engine(move |e, now| e.ensure_project(&id, &name, root.as_deref(), now))
            .await?
            .map_err(|e| e.to_string())
    }

    pub async fn project(&self, id: ProjectId) -> Result<Option<Project>, String> {
        self.with_engine(move |e, _| crate::store::project(e.conn(), &id))
            .await?
            .map_err(|e| e.to_string())
    }

    /// The project last opened from `root`, if any.
    pub async fn project_by_root(&self, root: &str) -> Result<Option<Project>, String> {
        let root = root.to_string();
        self.with_engine(move |e, _| crate::store::project_by_root(e.conn(), &root))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Every project, oldest first.
    pub async fn projects(&self) -> Result<Vec<Project>, String> {
        self.with_engine(|e, _| crate::store::projects(e.conn()))
            .await?
            .map_err(|e| e.to_string())
    }

    /// The migrated "Imported" project, while no folder has adopted it. Never
    /// a project with a folder.
    pub async fn unclaimed_import(&self) -> Result<Option<Project>, String> {
        self.with_engine(|e, _| crate::store::unclaimed_import(e.conn()))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Makes the unclaimed import `id` the project of the folder `root`,
    /// named `name`, once. Fails if `id` is not (or no longer) an unclaimed
    /// import without a folder. Grants no one anything.
    pub async fn adopt_import(
        &self,
        id: ProjectId,
        name: &str,
        root: &str,
    ) -> Result<Project, String> {
        let (name, root) = (name.to_string(), root.to_string());
        self.with_engine(move |e, _| e.adopt_import(&id, &name, &root))
            .await?
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "that history is no longer available to adopt".to_string())
    }

    /// Whether anything was ever said in `id`'s channels.
    pub async fn project_has_messages(&self, id: ProjectId) -> Result<bool, String> {
        self.with_engine(move |e, _| crate::store::project_has_messages(e.conn(), &id))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Deletes `id` if it has no folder, no messages and isn't the unclaimed
    /// import (see `store::forget_empty_project`). Returns whether it went.
    pub async fn forget_empty_project(&self, id: ProjectId) -> Result<bool, String> {
        self.with_engine(move |e, _| e.forget_empty_project(&id))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Renames a project. Its id, channels and history stay.
    pub async fn rename_project(&self, id: ProjectId, name: &str) -> Result<(), String> {
        let name = name.to_string();
        self.with_engine(move |e, _| crate::store::rename_project(e.conn(), &id, &name))
            .await?
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The channel `name` in `project`, created if new. Channel names are
    /// only unique within a project.
    pub async fn channel(&self, project: &ProjectId, name: &str) -> Result<Channel, String> {
        let (project, name) = (project.clone(), name.to_string());
        self.with_engine(move |e, now| e.ensure_channel(&project, &name, now))
            .await?
            .map_err(|e| e.to_string())
    }

    /// A project's channels, in `::N` order.
    pub async fn channels(&self, project: &ProjectId) -> Result<Vec<Channel>, String> {
        let project = project.clone();
        self.with_engine(move |e, _| crate::store::channels_in_project(e.conn(), &project))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Renames a channel. Its id, `::N` and history stay.
    pub async fn rename_channel(&self, channel: ChannelId, name: &str) -> Result<(), String> {
        let name = name.to_string();
        self.with_engine(move |e, _| crate::store::rename_channel(e.conn(), channel, &name))
            .await?
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Grants `member` membership of `project` (the permission to join its
    /// channels).
    pub async fn join_project(&self, project: &ProjectId, member: MemberId) -> Result<(), String> {
        let project = project.clone();
        self.with_engine(move |e, now| e.join_project(&project, member, now))
            .await?
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Joins `member` to `channel`. Refused (`not_a_project_member`) unless
    /// the member already belongs to the channel's project.
    pub async fn join(&self, member: MemberId, channel: ChannelId) -> Result<(), String> {
        self.with_engine(move |e, now| {
            let m = crate::store::member(e.conn(), member)?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            e.join(channel, &m, now)
        })
        .await?
        .map_err(|e| e.to_string())
    }

    /// A human member, created if new, made a member of `channel`'s project
    /// and joined to `channel`. Calling this is the host granting that
    /// person the project (the person at the keyboard, for the app).
    pub async fn human(
        &self,
        handle: &str,
        name: &str,
        channel: ChannelId,
    ) -> Result<Member, String> {
        let (handle, name) = (handle.to_string(), name.to_string());
        self.with_engine(move |e, now| {
            let m = e.ensure_member(MemberKind::Human, &handle, &name)?;
            let ch = crate::store::channel(e.conn(), channel)?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            e.join_project(&ch.project, m.id, now)?;
            e.join(channel, &m, now)?;
            Ok::<_, rusqlite::Error>(m)
        })
        .await?
        .map_err(|e| e.to_string())
    }

    /// Adds an agent to `channel` (joins it with the backfill cursor and runs
    /// the adapter's probe). No session starts until the agent is woken.
    ///
    /// This is the host's explicit grant: the agent also becomes a member of
    /// the channel's project. Nothing an agent does can reach this path.
    pub async fn add_agent(&self, spec: AgentSpec, channel: ChannelId) -> Result<Member, String> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(Cmd::AddAgent {
                spec,
                channel,
                reply,
            })
            .map_err(|_| GONE.to_string())?;
        wait.await.map_err(|_| GONE.to_string())?
    }

    /// Adds a model member to `channel`'s project roster and joins it to
    /// `channel`, like [`Core::add_agent`]. Idempotent: the same harness and
    /// model is always the same member, so this also re-registers roster
    /// members at startup. One harness can host many members (one adapter
    /// each).
    pub async fn add_model_member(
        &self,
        spec: MemberSpec,
        channel: ChannelId,
    ) -> Result<crate::roster::ModelMember, String> {
        let (harness, model, name) = (
            spec.harness.clone(),
            spec.model.clone(),
            spec.model_name.clone(),
        );
        let mm = self
            .with_engine(move |e, now| {
                let project = crate::store::channel(e.conn(), channel)?
                    .ok_or(rusqlite::Error::QueryReturnedNoRows)?
                    .project;
                e.store_tx(|c| {
                    let mm =
                        crate::roster::ensure_model_member(c, &harness, model.as_deref(), &name)?;
                    crate::roster::roster_add(c, &project, mm.member.id, now)?;
                    Ok::<_, rusqlite::Error>(mm)
                })
            })
            .await?
            .map_err(|e| e.to_string())?;
        self.add_agent(
            AgentSpec {
                handle: mm.member.handle.to_string(),
                name: mm.model_name.to_string(),
                adapter: spec.adapter,
                cwd: spec.cwd,
                runs_in: spec.runs_in,
            },
            channel,
        )
        .await?;
        Ok(mm)
    }

    /// Takes a member off `project`'s roster and out of its channels; any
    /// session it had there is interrupted and closed, and anything still
    /// owed to it is dropped (see `roster::leave_project`). Its messages
    /// stay.
    pub async fn remove_member(&self, project: &ProjectId, member: MemberId) -> Result<(), String> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(Cmd::RemoveMember {
                project: project.clone(),
                member,
                reply,
            })
            .map_err(|_| GONE.to_string())?;
        wait.await.map_err(|_| GONE.to_string())?
    }

    /// The project's model members, in the order they were added.
    pub async fn roster(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<crate::roster::ModelMember>, String> {
        let project = project.clone();
        self.with_engine(move |e, _| crate::roster::roster(e.conn(), &project))
            .await?
            .map_err(|e| e.to_string())
    }

    /// True the first time it is asked for `project`: the caller then fills
    /// the roster from auto-detection. Afterwards the roster is the user's.
    pub async fn seed_roster_once(&self, project: &ProjectId) -> Result<bool, String> {
        let project = project.clone();
        self.with_engine(move |e, now| {
            e.store_tx(|c| crate::roster::mark_seeded(c, &project, now))
        })
        .await?
        .map_err(|e| e.to_string())
    }

    /// The harness's cached model list (empty until it reported one).
    pub async fn harness_models(
        &self,
        harness: &str,
    ) -> Result<Vec<crate::harness::ModelInfo>, String> {
        let harness = harness.to_string();
        self.with_engine(move |e, _| crate::roster::harness_models(e.conn(), &harness))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Asks `adapter` for its harness's models (`AgentAdapter::list_models`)
    /// and caches a non-empty answer. Spawns a process: call it when the
    /// user is about to choose a model, not on every start.
    pub async fn discover_models(
        &self,
        harness: &str,
        adapter: Arc<dyn AgentAdapter>,
        cwd: PathBuf,
    ) -> Result<Vec<crate::harness::ModelInfo>, String> {
        let found = match adapter.list_models(cwd).await {
            None => return Ok(Vec::new()),
            Some(r) => r.map_err(|e| e.to_string())?,
        };
        if !found.is_empty() {
            let (h, models) = (harness.to_string(), found.clone());
            self.with_engine(move |e, now| {
                e.store_tx(|c| crate::roster::set_harness_models(c, &h, &models, now))
            })
            .await?
            .map_err(|e| e.to_string())?;
        }
        Ok(found)
    }

    pub async fn post(
        &self,
        author: MemberId,
        channel: ChannelId,
        body: &str,
        reply_to: Option<MessageId>,
    ) -> Result<Message, PostError> {
        let body = body.to_string();
        self.with_engine(move |e, now| {
            e.post(
                now,
                PostRequest {
                    author,
                    channel,
                    body: &body,
                    reply_to,
                    origin: Origin::Composer,
                    client_request_id: None,
                },
            )
            .map(|p| p.message)
        })
        .await
        .map_err(PostError::Store)?
    }

    pub async fn react(
        &self,
        member: MemberId,
        message: MessageId,
        emoji: &str,
    ) -> Result<(), PostError> {
        let emoji = emoji.to_string();
        self.with_engine(move |e, now| e.react(now, member, message, &emoji))
            .await
            .map_err(PostError::Store)?
    }

    /// A human refuses or dissents from `target` (agents use the chat tools).
    /// Posted as a reply carrying `reason`, recorded as a [`Stance`].
    pub async fn stance(
        &self,
        member: MemberId,
        channel: ChannelId,
        kind: StanceKind,
        target: MessageId,
        reason: &str,
    ) -> Result<Message, PostError> {
        let reason = reason.to_string();
        self.with_engine(move |e, now| {
            e.take_stance(
                now,
                StanceRequest {
                    author: member,
                    channel,
                    kind,
                    target,
                    reason: &reason,
                    model: None,
                    client_request_id: None,
                },
            )
            .map(|p| p.message)
        })
        .await
        .map_err(PostError::Store)?
    }

    /// The stance a message states, if any (for drawing it).
    pub async fn stance_of(&self, message: MessageId) -> Result<Option<Stance>, String> {
        self.with_engine(move |e, _| {
            crate::store::stance(e.conn(), message).map_err(|e| e.to_string())
        })
        .await?
    }

    pub async fn history(&self, channel: ChannelId, limit: usize) -> Result<Vec<Message>, String> {
        self.with_engine(move |e, _| e.history(channel, limit))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Records that a human was shown `channel` through `through` (None: the
    /// channel head). Moves their read receipt forward, never back.
    pub async fn mark_read(
        &self,
        member: MemberId,
        channel: ChannelId,
        through: Option<Seq>,
    ) -> Result<(), String> {
        self.with_engine(move |e, now| {
            e.advance_read(now, member, channel, through.unwrap_or(Seq::MAX))
        })
        .await
    }

    /// Who has read how far in `channel` (see [`Receipt`]).
    pub async fn receipts(&self, channel: ChannelId) -> Result<Vec<Receipt>, String> {
        self.with_engine(move |e, _| e.receipts(channel))
            .await?
            .map_err(|e| e.to_string())
    }

    pub async fn members(&self, channel: ChannelId) -> Result<Vec<Member>, String> {
        self.with_engine(move |e, _| crate::store::members_of(e.conn(), channel))
            .await?
            .map_err(|e| e.to_string())
    }

    pub async fn status(&self, agent: MemberId, channel: ChannelId) -> Result<AgentStatus, String> {
        self.with_engine(move |e, _| e.status(agent, channel)).await
    }

    /// Re-runs trigger creation for a message; a no-op for existing triggers.
    pub async fn retrigger(&self, message: MessageId) -> Result<(), String> {
        self.with_engine(move |e, now| e.retrigger(now, message))
            .await?
            .map_err(|e| e.to_string())
    }

    /// A human approves (`approve`) or denies a pending permission request.
    /// Any human member of the request's channel may answer; the answer is
    /// posted and delivered to the agent as a wake.
    pub async fn answer_permission(
        &self,
        request: PermissionId,
        by: MemberId,
        approve: bool,
        note: Option<&str>,
    ) -> Result<PermissionRequest, AnswerError> {
        let note = note.map(str::to_string);
        self.with_engine(move |e, now| {
            e.answer_permission(now, request, by, approve, note.as_deref())
        })
        .await
        .map_err(AnswerError::Store)?
    }

    /// Pending permission requests in `channel`, oldest first.
    pub async fn pending_permissions(
        &self,
        channel: ChannelId,
    ) -> Result<Vec<PermissionRequest>, String> {
        self.with_engine(move |e, _| e.pending_permissions(channel))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Calls a chat tool as the session `scope` would (tests, debugging).
    pub async fn tool(&self, scope: Scope, name: &str, args: Value) -> Result<Value, String> {
        let name = name.to_string();
        self.with_engine(move |e, now| mcp::call_tool(e, now, scope, &name, &args).map_err(|e| e.0))
            .await?
    }

    pub fn interrupt(&self, agent: MemberId, channel: ChannelId) {
        let _ = self.tx.send(Cmd::Interrupt { agent, channel });
    }

    /// Closes every session and stops the actor.
    pub async fn shutdown(&self) {
        let (reply, wait) = oneshot::channel();
        if self.tx.send(Cmd::Shutdown(reply)).is_ok() {
            let _ = wait.await;
        }
    }
}

// ---------- actor ----------

struct AgentEntry {
    handle: Arc<str>,
    adapter: Arc<dyn AgentAdapter>,
    cwd: PathBuf,
}

enum SlotState {
    Opening {
        queued: Option<Dispatch>,
    },
    Ready {
        session: Arc<dyn AgentSession>,
        fresh: bool,
    },
}

struct Slot {
    epoch: u64,
    token: String,
    state: SlotState,
}

struct Actor {
    engine: Engine,
    clock: Arc<dyn Clock>,
    tokens: Arc<Tokens>,
    mcp_url: String,
    bridge: Option<BridgeCommand>,
    default_cwd: PathBuf,
    agents: HashMap<MemberId, AgentEntry>,
    slots: HashMap<(MemberId, ChannelId), Slot>,
    next_epoch: u64,
    tx: mpsc::UnboundedSender<Cmd>,
    events: mpsc::UnboundedSender<CoreEvent>,
    alive: bool,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        while self.alive {
            let sleep = match self.engine.next_deadline() {
                Some(t) => self.clock.sleep_until(t),
                None => Box::pin(futures::future::pending()),
            };
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => break,
                },
                _ = sleep => {
                    let now = self.clock.now();
                    self.engine.tick(now);
                }
            }
            self.apply_effects();
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        let now = self.clock.now();
        match cmd {
            Cmd::Engine(f) => f(&mut self.engine, now),
            Cmd::AddAgent {
                spec,
                channel,
                reply,
            } => {
                let r = self.add_agent(spec, channel, now);
                let _ = reply.send(r);
            }
            Cmd::RemoveMember {
                project,
                member,
                reply,
            } => {
                let r = self.remove_member(&project, member, now);
                let _ = reply.send(r);
            }
            Cmd::Opened {
                agent,
                channel,
                epoch,
                result,
            } => self.opened(agent, channel, epoch, result, now),
            Cmd::Event {
                agent,
                channel,
                epoch,
                event,
            } => {
                if self.slots.get(&(agent, channel)).map(|s| s.epoch) != Some(epoch) {
                    return; // from a session that is already gone
                }
                let exited = matches!(event, AgentEvent::Exited { .. });
                self.engine.agent_event(now, agent, channel, event);
                if exited && let Some(slot) = self.slots.remove(&(agent, channel)) {
                    self.tokens.revoke(&slot.token);
                }
            }
            Cmd::DeliverFailed {
                agent,
                channel,
                epoch,
                batch,
                error,
            } => {
                if self.slots.get(&(agent, channel)).map(|s| s.epoch) == Some(epoch) {
                    self.engine.delivery_failed(
                        now,
                        agent,
                        channel,
                        batch,
                        error.kind,
                        &error.message,
                    );
                }
            }
            Cmd::Interrupt { agent, channel } => {
                if let Some(Slot {
                    state: SlotState::Ready { session, .. },
                    ..
                }) = self.slots.get(&(agent, channel))
                {
                    let session = session.clone();
                    tokio::spawn(async move {
                        let _ = session.interrupt().await;
                    });
                }
            }
            Cmd::Shutdown(reply) => {
                for (_, slot) in self.slots.drain() {
                    self.tokens.revoke(&slot.token);
                    if let SlotState::Ready { session, .. } = slot.state {
                        tokio::spawn(async move { session.close().await });
                    }
                }
                self.alive = false;
                let _ = reply.send(());
            }
        }
    }

    fn add_agent(
        &mut self,
        spec: AgentSpec,
        channel: ChannelId,
        now: Millis,
    ) -> Result<Member, String> {
        let member = self
            .engine
            .ensure_member(MemberKind::Agent, &spec.handle, &spec.name)
            .map_err(|e| e.to_string())?;
        let project = crate::store::channel(self.engine.conn(), channel)
            .map_err(|e| e.to_string())?
            .ok_or("no such channel")?
            .project;
        self.engine
            .join_project(&project, member.id, now)
            .map_err(|e| e.to_string())?;
        self.engine
            .join(channel, &member, now)
            .map_err(|e| e.to_string())?;
        self.engine.set_runs_in(member.id, spec.runs_in);
        let adapter = spec.adapter.clone();
        self.agents.insert(
            member.id,
            AgentEntry {
                handle: member.handle.clone(),
                adapter: spec.adapter,
                cwd: spec.cwd.unwrap_or_else(|| self.default_cwd.clone()),
            },
        );
        let tx = self.tx.clone();
        let id = member.id;
        tokio::spawn(async move {
            let probe = adapter.probe().await;
            let f: EngineFn = Box::new(move |e, _| e.set_probe(id, channel, &probe));
            let _ = tx.send(Cmd::Engine(f));
        });
        Ok(member)
    }

    fn remove_member(
        &mut self,
        project: &ProjectId,
        member: MemberId,
        now: Millis,
    ) -> Result<(), String> {
        let channels = self
            .engine
            .leave_project(now, project, member)
            .map_err(|e| e.to_string())?;
        for ch in channels {
            if let Some(slot) = self.slots.remove(&(member, ch)) {
                self.tokens.revoke(&slot.token);
                if let SlotState::Ready { session, .. } = slot.state {
                    tokio::spawn(async move {
                        let _ = session.interrupt().await;
                        session.close().await;
                    });
                }
            }
        }
        let still_somewhere = crate::roster::project_count(self.engine.conn(), member)
            .map_err(|e| e.to_string())?
            > 0;
        if !still_somewhere {
            self.agents.remove(&member);
        }
        Ok(())
    }

    fn apply_effects(&mut self) {
        loop {
            let effects = self.engine.take_effects();
            if effects.is_empty() {
                break;
            }
            for effect in effects {
                match effect {
                    Effect::Event(e) => {
                        let _ = self.events.send(e);
                    }
                    Effect::Dispatch(d) => self.dispatch(d),
                    Effect::Close { agent, channel } => {
                        if let Some(slot) = self.slots.remove(&(agent, channel)) {
                            self.tokens.revoke(&slot.token);
                            if let SlotState::Ready { session, .. } = slot.state {
                                tokio::spawn(async move { session.close().await });
                            }
                        }
                        self.engine.session_closed(agent, channel);
                    }
                }
            }
        }
    }

    fn dispatch(&mut self, d: Dispatch) {
        let key = (d.agent, d.channel);
        match self.slots.get_mut(&key) {
            Some(Slot {
                state: SlotState::Ready { session, fresh },
                epoch,
                ..
            }) => {
                let prompt = if *fresh {
                    Arc::from(format!("{}\n{}", d.orientation, d.prompt))
                } else {
                    d.prompt.clone()
                };
                *fresh = false;
                let (session, epoch) = (session.clone(), *epoch);
                self.deliver(session, epoch, d, prompt);
            }
            Some(Slot {
                state: SlotState::Opening { queued },
                ..
            }) => *queued = Some(d),
            None => self.open(d),
        }
    }

    fn deliver(&self, session: Arc<dyn AgentSession>, epoch: u64, d: Dispatch, prompt: Arc<str>) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Err(error) = session.deliver(d.batch, prompt).await {
                let _ = tx.send(Cmd::DeliverFailed {
                    agent: d.agent,
                    channel: d.channel,
                    epoch,
                    batch: d.batch,
                    error,
                });
            }
        });
    }

    fn open(&mut self, d: Dispatch) {
        let (agent, channel) = (d.agent, d.channel);
        let Some(entry) = self.agents.get(&agent) else {
            let now = self.clock.now();
            self.engine.delivery_failed(
                now,
                agent,
                channel,
                d.batch,
                AgentErrorKind::Spawn,
                "this agent is not configured in this run",
            );
            return;
        };
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        let token = self.tokens.mint(Scope {
            agent,
            channel,
            epoch,
        });
        let tx = self.tx.clone();
        let sink = EventSink::new(move |event| {
            let _ = tx.send(Cmd::Event {
                agent,
                channel,
                epoch,
                event,
            });
        });
        // Native permission requests are decided by the engine, in the actor,
        // and answered by the adapter at once (they never block on a human).
        let gate_tx = self.tx.clone();
        let permissions = PermissionGate::new(move |ask| {
            let tx = gate_tx.clone();
            Box::pin(async move {
                let (reply, wait) = oneshot::channel();
                let f: EngineFn = Box::new(move |e, now| {
                    let _ = reply.send(e.native_permission(now, agent, channel, ask));
                });
                if tx.send(Cmd::Engine(f)).is_err() {
                    return PermissionVerdict::Decline;
                }
                wait.await.unwrap_or(PermissionVerdict::Decline)
            })
        });
        let req = OpenSession {
            permissions,
            agent,
            channel,
            handle: entry.handle.clone(),
            cwd: entry.cwd.clone(),
            chat_tools: ChatToolsEndpoint {
                url: self.mcp_url.clone(),
                token: token.clone(),
                bridge: self.bridge.clone(),
            },
            events: sink,
        };
        let adapter = entry.adapter.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = adapter.open(req).await;
            let _ = tx.send(Cmd::Opened {
                agent,
                channel,
                epoch,
                result,
            });
        });
        self.slots.insert(
            (agent, channel),
            Slot {
                epoch,
                token,
                state: SlotState::Opening { queued: Some(d) },
            },
        );
    }

    fn opened(
        &mut self,
        agent: MemberId,
        channel: ChannelId,
        epoch: u64,
        result: Result<Arc<dyn AgentSession>, AgentError>,
        now: Millis,
    ) {
        let key = (agent, channel);
        if self.slots.get(&key).map(|s| s.epoch) != Some(epoch) {
            if let Ok(session) = result {
                tokio::spawn(async move { session.close().await });
            }
            return;
        }
        match result {
            Ok(session) => {
                self.engine
                    .session_opened(now, agent, channel, session.info());
                let slot = self.slots.get_mut(&key).expect("checked");
                let queued = match std::mem::replace(
                    &mut slot.state,
                    SlotState::Ready {
                        session: session.clone(),
                        fresh: true,
                    },
                ) {
                    SlotState::Opening { queued } => queued,
                    SlotState::Ready { .. } => None,
                };
                if let Some(d) = queued {
                    self.dispatch(d);
                }
            }
            Err(error) => {
                let slot = self.slots.remove(&key).expect("checked");
                self.tokens.revoke(&slot.token);
                if let SlotState::Opening { queued: Some(d) } = slot.state {
                    self.engine.delivery_failed(
                        now,
                        agent,
                        channel,
                        d.batch,
                        error.kind,
                        &error.message,
                    );
                }
                self.engine.session_closed(agent, channel);
            }
        }
    }
}
