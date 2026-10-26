//! The in-process runtime: one actor task owns the [`Engine`], the agent
//! sessions and the chat MCP server, behind the cloneable [`Core`] handle.
//!
//! The handle is the API boundary that could later move behind a daemon: every
//! call is a message to the actor, every answer comes back on a channel, and UI
//! updates arrive as [`CoreEvent`]s.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    adapter::*,
    engine::{AgentStatus, CoreEvent, Dispatch, Effect, Engine, PostError, PostRequest},
    mcp::{self, Scope, Tokens},
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
    Shutdown(oneshot::Sender<()>),
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

    pub async fn channel(&self, name: &str) -> Result<Channel, String> {
        let name = name.to_string();
        self.with_engine(move |e, now| e.ensure_channel(&name, now))
            .await?
            .map_err(|e| e.to_string())
    }

    /// A human member, created if new, joined to `channel`.
    pub async fn human(&self, handle: &str, name: &str, channel: ChannelId) -> Result<Member, String> {
        let (handle, name) = (handle.to_string(), name.to_string());
        self.with_engine(move |e, now| {
            let m = e.ensure_member(MemberKind::Human, &handle, &name)?;
            e.join(channel, &m, now)?;
            Ok::<_, rusqlite::Error>(m)
        })
        .await?
        .map_err(|e| e.to_string())
    }

    /// Adds an agent to `channel` (joins it with the backfill cursor and runs
    /// the adapter's probe). No session starts until the agent is woken.
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

    pub async fn react(&self, member: MemberId, message: MessageId, emoji: &str) -> Result<(), PostError> {
        let emoji = emoji.to_string();
        self.with_engine(move |e, now| e.react(now, member, message, &emoji))
            .await
            .map_err(PostError::Store)?
    }

    pub async fn history(&self, channel: ChannelId, limit: usize) -> Result<Vec<Message>, String> {
        self.with_engine(move |e, _| e.history(channel, limit))
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
    Opening { queued: Option<Dispatch> },
    Ready { session: Arc<dyn AgentSession>, fresh: bool },
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
                    self.engine
                        .delivery_failed(now, agent, channel, batch, error.kind, &error.message);
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

    fn add_agent(&mut self, spec: AgentSpec, channel: ChannelId, now: Millis) -> Result<Member, String> {
        let member = self
            .engine
            .ensure_member(MemberKind::Agent, &spec.handle, &spec.name)
            .map_err(|e| e.to_string())?;
        self.engine
            .join(channel, &member, now)
            .map_err(|e| e.to_string())?;
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
        let req = OpenSession {
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
                self.engine.session_opened(now, agent, channel, session.info());
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
                    self.engine
                        .delivery_failed(now, agent, channel, d.batch, error.kind, &error.message);
                }
                self.engine.session_closed(agent, channel);
            }
        }
    }
}
