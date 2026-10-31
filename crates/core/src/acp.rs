//! Generic ACP adapter (Agent Client Protocol, `agent-client-protocol` crate).
//!
//! # Version negotiation
//!
//! [`AgentAdapter::probe`] only checks that the agent command plausibly exists
//! (no spawn). [`AgentAdapter::open`] settles the protocol version: with
//! `prefer_v2` it first sends a **v2 draft** `initialize`; if the agent selects
//! v2 the session runs on the v2 driver, and if it answers with v1 (or rejects
//! v2) the connection is dropped and a fresh one is opened with a v1
//! `initialize`. The selected version is cached per adapter so later sessions
//! skip straight to it. v2 is behind the crate's `unstable_protocol_v2` feature
//! and is marked `acp/v2-draft` everywhere it surfaces.
//!
//! Capabilities come from the live `initialize` (+ `session/new`) answer. ACP
//! has no standard steering or queueing, so [`Caps::STEER`] and [`Caps::QUEUE`]
//! are never set here; vendor extensions that offer them (claude-agent-acp's
//! `_session/steering`, `promptQueueing`) are listed in
//! `SessionInfo::unused_extensions` and not used.
//!
//! # Acceptance point
//!
//! - **v1**: `session/prompt` runs until the turn ends, so its response cannot
//!   be the acceptance signal. A turn is *accepted* when the prompt request has
//!   been handed to the transport **and** then one of these arrives for the
//!   session: a turn-content `session/update` (`user_message_chunk`,
//!   `agent_message_chunk`, `agent_thought_chunk`, `tool_call`,
//!   `tool_call_update`, `plan`), a `session/request_permission`, or a
//!   successful prompt response. Housekeeping updates (`available_commands_update`,
//!   `current_mode_update`, `config_option_update`, `session_info_update`,
//!   `usage_update`) do not count: agents send those on their own. An error
//!   response before any of these means "not accepted" (retryable).
//! - **v2 draft**: the `session/prompt` response acknowledges acceptance; the
//!   turn ends at the next `state_update: idle`.
//!
//! # Permissions
//!
//! Chaos Control never changes the agent's permission mode. A
//! `session/request_permission` goes to core through [`OpenSession::permissions`]
//! and is answered at once with what core decides (`crate::permission`): the
//! agent's own allow-once option (VM agent, or a human's one-shot grant), else
//! its own "not now" decline (a reject-once that lets the turn continue) with a
//! `_meta.chaosControl` note that request #N was sent to the humans, or
//! `cancelled` if it offered no decline.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use agent_client_protocol::{
    self as acp, Agent, Client, ConnectTo, ConnectionTo, DynConnectTo,
    schema::{ProtocolVersion, v1},
};
use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

use crate::adapter::*;
use crate::harness::ModelInfo;
use crate::permission::{
    OptionKind, PermissionAsk, PermissionVerdict, pick_allow, pick_decline, sent_meta,
};
use crate::types::BatchId;

/// How to reach the agent process.
pub trait AcpTransport: Send + Sync {
    fn describe(&self) -> String;
    fn probe(&self) -> Probe;
    /// A fresh connection (spawns a process for a real agent).
    fn connect(&self) -> Result<DynConnectTo<Client>, AgentError>;
}

/// Spawns an agent binary and speaks ACP over its stdio.
pub struct SpawnTransport {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Append every frame as NDJSON `{dir, frame}` (replay-fixture format).
    pub record: Option<PathBuf>,
}

impl AcpTransport for SpawnTransport {
    fn describe(&self) -> String {
        format!("acp: {} {}", self.command.display(), self.args.join(" "))
    }

    fn probe(&self) -> Probe {
        if resolve_command(&self.command).is_some() {
            Probe::Plausible
        } else {
            Probe::Missing {
                why: Arc::from(format!("`{}` not found on PATH", self.command.display())),
            }
        }
    }

    fn connect(&self) -> Result<DynConnectTo<Client>, AgentError> {
        let mut config = acp::AcpAgentConfig::new(self.command.clone()).args(self.args.clone());
        for (k, v) in &self.env {
            config = config.env(k.clone(), v.clone());
        }
        let mut agent = acp::AcpAgent::new(config);
        if let Some(path) = self.record.clone() {
            let file = Arc::new(Mutex::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| AgentError::new(AgentErrorKind::Spawn, e.to_string()))?,
            ));
            agent = agent.with_debug(move |line, direction| {
                use std::io::Write;
                let dir = match direction {
                    acp::LineDirection::Stdin => "out",
                    acp::LineDirection::Stdout => "in",
                    _ => "stderr",
                };
                let frame = serde_json::from_str::<serde_json::Value>(line)
                    .unwrap_or_else(|_| serde_json::Value::String(line.to_string()));
                if let Ok(mut f) = file.lock() {
                    let _ = writeln!(f, "{}", serde_json::json!({ "dir": dir, "frame": frame }));
                }
            });
        }
        Ok(DynConnectTo::new(agent))
    }
}

/// `command` as given if it has a path separator, else the first match on PATH.
pub fn resolve_command(command: &std::path::Path) -> Option<PathBuf> {
    if command.components().count() > 1 {
        return command.exists().then(|| command.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|p| p.is_file())
}

/// An in-process transport, e.g. a scripted agent in tests.
pub struct FnTransport<F>(pub String, pub F);

impl<F> AcpTransport for FnTransport<F>
where
    F: Fn() -> DynConnectTo<Client> + Send + Sync,
{
    fn describe(&self) -> String {
        self.0.clone()
    }
    fn probe(&self) -> Probe {
        Probe::Plausible
    }
    fn connect(&self) -> Result<DynConnectTo<Client>, AgentError> {
        Ok((self.1)())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatToolsPreference {
    /// HTTP if the agent advertises `mcpCapabilities.http`, else the stdio bridge.
    Auto,
    /// Always the stdio bridge (for agents that advertise HTTP but drop it).
    StdioBridge,
}

pub struct AcpAdapter {
    transport: Arc<dyn AcpTransport>,
    prefer_v2: bool,
    chat_tools: ChatToolsPreference,
    /// The version the agent selected last time (1 or 2).
    negotiated: Mutex<Option<u16>>,
    /// The model member's model, chosen through the session's model config
    /// option after `session/new`. `None` runs the harness default.
    model: Option<Arc<str>>,
}

impl AcpAdapter {
    pub fn new(transport: Arc<dyn AcpTransport>) -> Self {
        AcpAdapter {
            transport,
            prefer_v2: true,
            chat_tools: ChatToolsPreference::Auto,
            negotiated: Mutex::new(None),
            model: None,
        }
    }

    /// Runs sessions on `model` (the harness's own id), set through ACP's
    /// `session/set_config_option` on the session's model option.
    pub fn model(mut self, model: Option<Arc<str>>) -> Self {
        self.model = model;
        self
    }

    pub fn prefer_v2(mut self, yes: bool) -> Self {
        self.prefer_v2 = yes;
        self
    }

    pub fn chat_tools(mut self, pref: ChatToolsPreference) -> Self {
        self.chat_tools = pref;
        self
    }

    /// The protocol version settled by the last `open` (None before any).
    pub fn negotiated_version(&self) -> Option<u16> {
        *self.negotiated.lock().expect("lock")
    }
}

impl AgentAdapter for AcpAdapter {
    fn describe(&self) -> String {
        self.transport.describe()
    }

    fn probe(&self) -> BoxFuture<'_, Probe> {
        Box::pin(async move { self.transport.probe() })
    }

    fn open(&self, req: OpenSession) -> BoxFuture<'_, Result<Arc<dyn AgentSession>, AgentError>> {
        Box::pin(async move {
            let cached = self.negotiated_version();
            if self.prefer_v2 && cached != Some(1) {
                match crate::acp_v2::open(
                    self.transport.connect()?,
                    &req,
                    self.chat_tools,
                    self.model.clone(),
                )
                .await
                {
                    Ok(session) => {
                        *self.negotiated.lock().expect("lock") = Some(2);
                        return Ok(session);
                    }
                    Err(V2Open::Fallback) => {
                        *self.negotiated.lock().expect("lock") = Some(1);
                    }
                    Err(V2Open::Failed(e)) => return Err(e),
                }
            }
            let session = open_v1(
                self.transport.connect()?,
                req,
                self.chat_tools,
                self.model.clone(),
            )
            .await?;
            *self.negotiated.lock().expect("lock") = Some(1);
            Ok(session)
        })
    }

    fn list_models(
        &self,
        cwd: PathBuf,
    ) -> BoxFuture<'_, Option<Result<Vec<ModelInfo>, AgentError>>> {
        Box::pin(async move {
            let transport = match self.transport.connect() {
                Ok(t) => t,
                Err(e) => return Some(Err(e)),
            };
            let found = tokio::time::timeout(DISCOVERY_TIMEOUT, discover_v1(transport, cwd))
                .await
                .unwrap_or_else(|_| {
                    Err(AgentError::new(
                        AgentErrorKind::Transport,
                        "no answer while listing models",
                    ))
                });
            Some(found)
        })
    }
}

/// How long model discovery may take before the picker keeps the preset list.
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// ---------- models (session config options) ----------

/// The session's model selector, read from ACP `configOptions` (v1 and the
/// v2 draft share the JSON shape, so both are read through serde).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelOption {
    pub config_id: String,
    pub current: Option<String>,
    pub models: Vec<ModelInfo>,
}

/// The `select` option with category `model`, else the one with id `model`
/// (fx names it that way). Grouped options are flattened in order.
pub(crate) fn model_option(config_options: &serde_json::Value) -> Option<ModelOption> {
    let all = config_options.as_array()?;
    let is_select =
        |o: &&serde_json::Value| o.get("type").and_then(|t| t.as_str()) == Some("select");
    let pick = all
        .iter()
        .filter(is_select)
        .find(|o| o.get("category").and_then(|c| c.as_str()) == Some("model"))
        .or_else(|| {
            all.iter()
                .filter(is_select)
                .find(|o| o.get("id").and_then(|c| c.as_str()) == Some("model"))
        })?;
    let mut models = Vec::new();
    let mut push = |o: &serde_json::Value| {
        if let Some(value) = o.get("value").and_then(|v| v.as_str()) {
            let name = o.get("name").and_then(|v| v.as_str()).unwrap_or(value);
            models.push(ModelInfo::new(Some(value), name));
        }
    };
    for o in pick.get("options")?.as_array()? {
        match o.get("options").and_then(|g| g.as_array()) {
            Some(group) => group.iter().for_each(&mut push),
            None => push(o),
        }
    }
    Some(ModelOption {
        config_id: pick.get("id")?.as_str()?.to_string(),
        current: pick
            .get("currentValue")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        models,
    })
}

/// What to do about a member's model once `session/new` answered.
pub(crate) enum ModelPlan {
    /// Send `session/set_config_option` with these; `fallback` is the
    /// current model if that fails.
    Set {
        config_id: String,
        value: Arc<str>,
        fallback: Option<Arc<str>>,
    },
    Keep {
        current: Option<Arc<str>>,
        applied: Result<(), String>,
    },
}

pub(crate) fn plan_model(want: Option<&Arc<str>>, offered: Option<&ModelOption>) -> ModelPlan {
    let current = offered.and_then(|o| o.current.as_deref()).map(Arc::from);
    match (want, offered) {
        (Some(want), Some(opt)) if opt.models.iter().any(|m| m.id.as_deref() == Some(&**want)) => {
            if opt.current.as_deref() == Some(&**want) {
                return ModelPlan::Keep {
                    current,
                    applied: Ok(()),
                };
            }
            ModelPlan::Set {
                config_id: opt.config_id.clone(),
                value: want.clone(),
                fallback: current,
            }
        }
        (Some(_), None) => ModelPlan::Keep {
            current: None,
            applied: Err("it has no model option".into()),
        },
        _ => ModelPlan::Keep {
            current,
            applied: Ok(()),
        },
    }
}

/// Initialize, open a session with no chat tools, read its model option and
/// close it again.
async fn discover_v1(
    transport: DynConnectTo<Client>,
    cwd: PathBuf,
) -> Result<Vec<ModelInfo>, AgentError> {
    let (tx, rx) = oneshot::channel::<Result<Vec<ModelInfo>, AgentError>>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let send = move |r| {
        if let Some(tx) = tx.lock().expect("lock").take() {
            let _ = tx.send(r);
        }
    };
    let send_main = send.clone();
    let result = Client
        .builder()
        .name("chaos-control")
        .connect_with(transport, async move |conn: ConnectionTo<Agent>| {
            let init = conn
                .send_request(
                    v1::InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(v1::ClientCapabilities::new())
                        .client_info(v1::Implementation::new(
                            "chaos-control",
                            env!("CARGO_PKG_VERSION"),
                        )),
                )
                .block_task()
                .await;
            if let Err(e) = init {
                send_main(Err(AgentError::new(
                    error_kind(&e),
                    format!("initialize: {e}"),
                )));
                return Ok(());
            }
            let new = match conn
                .send_request(v1::NewSessionRequest::new(cwd))
                .block_task()
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    send_main(Err(AgentError::new(
                        error_kind(&e),
                        format!("session/new: {e}"),
                    )));
                    return Ok(());
                }
            };
            let options = serde_json::to_value(&new.config_options).unwrap_or_default();
            let models = model_option(&options).map(|m| m.models).unwrap_or_default();
            // Best effort: agents without session/close just exit with us.
            let _ = conn
                .send_request(v1::CloseSessionRequest::new(new.session_id.clone()))
                .block_task()
                .await;
            send_main(Ok(models));
            Ok(())
        })
        .await;
    send(Err(AgentError::new(
        AgentErrorKind::Spawn,
        match result {
            Err(e) => format!("{e}"),
            Ok(()) => "agent exited while listing models".into(),
        },
    )));
    rx.await
        .unwrap_or_else(|_| Err(AgentError::new(AgentErrorKind::Transport, "agent exited")))
}

/// Outcome of trying the v2 draft.
pub enum V2Open {
    /// The agent selected v1 (or refused v2): reconnect with v1.
    Fallback,
    Failed(AgentError),
}

// ---------- shared turn tracking ----------

#[derive(Default)]
pub(crate) struct Tracker {
    pub turn: Option<BatchId>,
    pub accepted: bool,
    /// Assistant text since the last tool call: the turn's final message.
    pub text: String,
    /// v2: the agent messages since the last new tool call, by `messageId`,
    /// in arrival order (chunks append, an `agent_message` snapshot replaces).
    pub messages: Vec<(String, String)>,
    /// v2: tool call ids seen this turn (a repeat is an upsert, not a new call).
    pub calls: std::collections::HashSet<String>,
}

impl Tracker {
    /// Marks the in-flight turn accepted; returns the event to emit, once.
    pub fn accept(&mut self) -> Option<AgentEvent> {
        match self.turn {
            Some(turn) if !self.accepted => {
                self.accepted = true;
                Some(AgentEvent::Accepted { turn })
            }
            _ => None,
        }
    }
}

pub(crate) fn auth_like(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("auth") || m.contains("login") || m.contains("log in") || m.contains("sign in")
}

pub(crate) fn error_kind(e: &acp::Error) -> AgentErrorKind {
    let text = format!("{e}");
    if auth_like(&text) || i32::from(e.code) == -32000 {
        AgentErrorKind::Auth
    } else {
        AgentErrorKind::Protocol
    }
}

pub(crate) fn choose_mcp(
    pref: ChatToolsPreference,
    http_ok: bool,
    ep: &ChatToolsEndpoint,
) -> ChatToolsTransport {
    match (pref, http_ok, ep.bridge.is_some()) {
        (ChatToolsPreference::Auto, true, _) => ChatToolsTransport::Http,
        (_, _, true) => ChatToolsTransport::StdioBridge,
        (ChatToolsPreference::Auto, false, false) => ChatToolsTransport::None,
        (ChatToolsPreference::StdioBridge, _, false) => {
            if http_ok {
                ChatToolsTransport::Http
            } else {
                ChatToolsTransport::None
            }
        }
    }
}

// ---------- v1 ----------

enum Cmd {
    Prompt {
        turn: BatchId,
        text: Arc<str>,
        done: oneshot::Sender<Result<(), AgentError>>,
    },
    Cancel,
    Close,
}

struct AcpSession {
    info: SessionInfo,
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl AgentSession for AcpSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    fn deliver(&self, turn: BatchId, prompt: Arc<str>) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            let (done, rx) = oneshot::channel();
            self.cmd
                .send(Cmd::Prompt {
                    turn,
                    text: prompt,
                    done,
                })
                .map_err(|_| AgentError::new(AgentErrorKind::Transport, "session is closed"))?;
            rx.await
                .map_err(|_| AgentError::new(AgentErrorKind::Transport, "session is closed"))?
        })
    }

    fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            self.cmd
                .send(Cmd::Cancel)
                .map_err(|_| AgentError::new(AgentErrorKind::Transport, "session is closed"))
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.cmd.send(Cmd::Close);
        })
    }
}

fn v1_caps(init: &v1::InitializeResponse) -> (Caps, Vec<Arc<str>>, bool) {
    let a = &init.agent_capabilities;
    let mut caps = Caps::CHAT_TOOLS_STDIO | Caps::INTERRUPT;
    let http = a.mcp_capabilities.http;
    if http {
        caps.insert(Caps::CHAT_TOOLS_HTTP);
    }
    if a.load_session || a.session_capabilities.resume.is_some() {
        caps.insert(Caps::RESUME);
    }
    let mut ext = Vec::new();
    let meta = serde_json::to_value(init).unwrap_or_default();
    if meta.pointer("/_meta/steering/supported") == Some(&serde_json::Value::Bool(true)) {
        ext.push(Arc::from("_session/steering (vendor steering)"));
    }
    if meta.pointer("/agentCapabilities/_meta/claudeCode/promptQueueing")
        == Some(&serde_json::Value::Bool(true))
    {
        ext.push(Arc::from(
            "_meta.claudeCode.promptQueueing (vendor queueing)",
        ));
    }
    if meta
        .pointer("/agentCapabilities/sessionCapabilities/systemPrompt")
        .is_some()
    {
        ext.push(Arc::from("sessionCapabilities.systemPrompt (unstable)"));
    }
    (caps, ext, http)
}

fn v1_mcp_servers(kind: ChatToolsTransport, ep: &ChatToolsEndpoint) -> Vec<v1::McpServer> {
    match kind {
        ChatToolsTransport::Http => vec![v1::McpServer::Http(
            v1::McpServerHttp::new("chaos-control", ep.url.clone()).headers(vec![
                v1::HttpHeader::new("Authorization", format!("Bearer {}", ep.token)),
            ]),
        )],
        ChatToolsTransport::StdioBridge => {
            let b = ep.bridge.as_ref().expect("bridge chosen only when present");
            vec![v1::McpServer::Stdio(
                v1::McpServerStdio::new("chaos-control", b.command.clone())
                    .args(b.args.clone())
                    .env(vec![
                        v1::EnvVariable::new("CHAOS_MCP_URL", ep.url.clone()),
                        v1::EnvVariable::new("CHAOS_MCP_TOKEN", ep.token.clone()),
                    ]),
            )]
        }
        ChatToolsTransport::None => Vec::new(),
    }
}

fn is_turn_content(u: &v1::SessionUpdate) -> bool {
    matches!(
        u,
        v1::SessionUpdate::UserMessageChunk(_)
            | v1::SessionUpdate::AgentMessageChunk(_)
            | v1::SessionUpdate::AgentThoughtChunk(_)
            | v1::SessionUpdate::ToolCall(_)
            | v1::SessionUpdate::ToolCallUpdate(_)
            | v1::SessionUpdate::Plan(_)
    )
}

fn v1_outcome(stop: v1::StopReason) -> TurnOutcome {
    match stop {
        v1::StopReason::EndTurn => TurnOutcome::Completed,
        v1::StopReason::Cancelled => TurnOutcome::Cancelled,
        v1::StopReason::Refusal => TurnOutcome::Refused,
        v1::StopReason::MaxTokens | v1::StopReason::MaxTurnRequests => TurnOutcome::MaxTokens,
        other => TurnOutcome::Other(Arc::from(format!("{other:?}"))),
    }
}

/// A tool kind as its wire string (`read`, `search`, `execute`, ...).
pub(crate) fn kind_str(k: &impl serde::Serialize) -> String {
    match serde_json::to_value(k) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn v1_kind(k: &v1::PermissionOptionKind) -> OptionKind {
    match k {
        v1::PermissionOptionKind::AllowOnce => OptionKind::AllowOnce,
        v1::PermissionOptionKind::AllowAlways => OptionKind::AllowAlways,
        v1::PermissionOptionKind::RejectOnce => OptionKind::RejectOnce,
        v1::PermissionOptionKind::RejectAlways => OptionKind::RejectAlways,
        _ => OptionKind::Other,
    }
}

/// Answers a permission request with the agent's own option for `verdict`.
fn v1_answer(
    req: &v1::RequestPermissionRequest,
    verdict: &PermissionVerdict,
) -> v1::RequestPermissionResponse {
    let opts: Vec<(OptionKind, &str)> = req
        .options
        .iter()
        .map(|o| (v1_kind(&o.kind), o.name.as_str()))
        .collect();
    let select = |ix: usize| {
        v1::RequestPermissionResponse::new(v1::RequestPermissionOutcome::Selected(
            v1::SelectedPermissionOutcome::new(req.options[ix].option_id.clone()),
        ))
    };
    let decline = || match pick_decline(&opts) {
        Some(ix) => select(ix),
        None => v1::RequestPermissionResponse::new(v1::RequestPermissionOutcome::Cancelled),
    };
    match verdict {
        PermissionVerdict::Allow { .. } | PermissionVerdict::AllowRead => match pick_allow(&opts) {
            Some(ix) => select(ix),
            None => decline(),
        },
        PermissionVerdict::Sent { request } => decline().meta(sent_meta(*request)),
        PermissionVerdict::Decline => decline(),
    }
}

async fn open_v1(
    transport: DynConnectTo<Client>,
    req: OpenSession,
    pref: ChatToolsPreference,
    model: Option<Arc<str>>,
) -> Result<Arc<dyn AgentSession>, AgentError> {
    let (ready_tx, ready_rx) = oneshot::channel::<Result<SessionInfo, AgentError>>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let tracker = Arc::new(Mutex::new(Tracker::default()));
    let events = req.events.clone();
    tokio::spawn(run_v1(
        transport, req, pref, model, tracker, cmd_rx, ready_tx, events,
    ));
    let info = ready_rx
        .await
        .map_err(|_| AgentError::new(AgentErrorKind::Transport, "agent exited during setup"))??;
    Ok(Arc::new(AcpSession { info, cmd: cmd_tx }))
}

#[allow(clippy::too_many_arguments)]
async fn run_v1(
    transport: DynConnectTo<Client>,
    req: OpenSession,
    pref: ChatToolsPreference,
    model: Option<Arc<str>>,
    tracker: Arc<Mutex<Tracker>>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    ready_tx: oneshot::Sender<Result<SessionInfo, AgentError>>,
    events: EventSink,
) {
    let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));
    let ready_for_main = ready_tx.clone();
    // Set once the session is handed out; only then do exits become events.
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started_main = started.clone();
    let t_notif = tracker.clone();
    let t_perm = tracker.clone();
    let ev_notif = events.clone();
    let ev_perm = events.clone();
    let ev_main = events.clone();
    let t_main = tracker.clone();
    let gate = req.permissions.clone();

    let result = Client
        .builder()
        .name("chaos-control")
        .on_receive_notification(
            async move |n: v1::SessionNotification, _cx| {
                let mut out = Vec::new();
                {
                    let mut t = t_notif.lock().expect("lock");
                    if t.turn.is_some() && is_turn_content(&n.update) {
                        out.extend(t.accept());
                    }
                    match &n.update {
                        v1::SessionUpdate::AgentMessageChunk(chunk) if t.turn.is_some() => {
                            if let v1::ContentBlock::Text(text) = &chunk.content {
                                t.text.push_str(&text.text);
                            }
                        }
                        v1::SessionUpdate::ToolCall(call) if t.turn.is_some() => {
                            t.text.clear();
                            out.push(AgentEvent::Activity {
                                label: Arc::from(call.title.as_str()),
                            });
                        }
                        _ => {}
                    }
                }
                for e in out {
                    ev_notif.emit(e);
                }
                Ok(())
            },
            acp::on_receive_notification!(),
        )
        .on_receive_request(
            async move |r: v1::RequestPermissionRequest, responder, _cx| {
                let (accepted, turn) = {
                    let mut t = t_perm.lock().expect("lock");
                    (t.accept(), t.turn)
                };
                if let Some(e) = accepted {
                    ev_perm.emit(e);
                }
                let title = r
                    .tool_call
                    .fields
                    .title
                    .clone()
                    .unwrap_or_else(|| "a tool".to_string());
                let detail = r
                    .tool_call
                    .fields
                    .raw_input
                    .as_ref()
                    .map(|v| Arc::from(v.to_string()));
                let read_only = r
                    .tool_call
                    .fields
                    .kind
                    .as_ref()
                    .is_some_and(|k| crate::permission::is_read_only_kind(&kind_str(k)));
                let verdict = gate
                    .ask(PermissionAsk {
                        turn,
                        title: Arc::from(title),
                        detail,
                        read_only,
                    })
                    .await;
                responder.respond(v1_answer(&r, &verdict))
            },
            acp::on_receive_request!(),
        )
        .connect_with(transport, async move |conn: ConnectionTo<Agent>| {
            let fail = |e: AgentError| {
                if let Some(tx) = ready_for_main.lock().expect("lock").take() {
                    let _ = tx.send(Err(e));
                }
            };
            let init = match conn
                .send_request(
                    v1::InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(v1::ClientCapabilities::new())
                        .client_info(v1::Implementation::new(
                            "chaos-control",
                            env!("CARGO_PKG_VERSION"),
                        )),
                )
                .block_task()
                .await
            {
                Ok(i) => i,
                Err(e) => {
                    fail(AgentError::new(error_kind(&e), format!("initialize: {e}")));
                    return Ok(());
                }
            };
            if init.protocol_version != ProtocolVersion::V1 {
                fail(AgentError::new(
                    AgentErrorKind::Protocol,
                    format!("agent selected ACP {}", init.protocol_version),
                ));
                return Ok(());
            }
            let (caps, unused_extensions, http_ok) = v1_caps(&init);
            let chat_tools = choose_mcp(pref, http_ok, &req.chat_tools);
            let new = match conn
                .send_request(
                    v1::NewSessionRequest::new(req.cwd.clone())
                        .mcp_servers(v1_mcp_servers(chat_tools, &req.chat_tools)),
                )
                .block_task()
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    fail(AgentError::new(error_kind(&e), format!("session/new: {e}")));
                    return Ok(());
                }
            };
            let session_id = new.session_id.clone();
            let options = serde_json::to_value(&new.config_options).unwrap_or_default();
            let offered = model_option(&options);
            let (current, applied) = match plan_model(model.as_ref(), offered.as_ref()) {
                ModelPlan::Set {
                    config_id,
                    value,
                    fallback,
                } => {
                    let set = conn
                        .send_request(v1::SetSessionConfigOptionRequest::new(
                            session_id.clone(),
                            config_id,
                            v1::SessionConfigValueId::new(value.clone()),
                        ))
                        .block_task()
                        .await;
                    match set {
                        Ok(_) => (Some(value), Ok(())),
                        Err(e) => (fallback, Err(format!("{e}"))),
                    }
                }
                ModelPlan::Keep { current, applied } => (current, applied),
            };
            let models = offered.map(|o| o.models).unwrap_or_default();
            let model_note = model_outcome(model.as_deref(), &models, applied);
            let info = SessionInfo {
                model: current,
                models,
                model_note,
                caps,
                protocol: Arc::from("acp/v1"),
                native_ref: Arc::from(session_id.to_string()),
                agent_name: init
                    .agent_info
                    .as_ref()
                    .map(|i| Arc::from(format!("{} {}", i.name, i.version))),
                unused_extensions,
                chat_tools,
            };
            if let Some(tx) = ready_for_main.lock().expect("lock").take() {
                started_main.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = tx.send(Ok(info));
            }
            loop {
                // The agent closing its end (process exit) ends the session.
                let cmd = tokio::select! {
                    cmd = cmd_rx.recv() => cmd,
                    _ = conn.incoming_closed() => None,
                };
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::Prompt { turn, text, done } => {
                        {
                            let mut t = t_main.lock().expect("lock");
                            *t = Tracker {
                                turn: Some(turn),
                                ..Tracker::default()
                            };
                        }
                        let sent = conn.send_request(v1::PromptRequest::new(
                            session_id.clone(),
                            vec![v1::ContentBlock::Text(v1::TextContent::new(
                                text.to_string(),
                            ))],
                        ));
                        // Handed to the transport: deliver() returns Ok now.
                        let _ = done.send(Ok(()));
                        let t = t_main.clone();
                        let ev = ev_main.clone();
                        tokio::spawn(async move {
                            let result = sent.block_task().await;
                            let mut out = Vec::new();
                            {
                                let mut tr = t.lock().expect("lock");
                                if tr.turn != Some(turn) {
                                    return; // already failed by connection loss
                                }
                                match result {
                                    Ok(resp) => {
                                        out.extend(tr.accept());
                                        out.push(AgentEvent::TurnEnded {
                                            turn,
                                            outcome: v1_outcome(resp.stop_reason),
                                            final_text: Arc::from(std::mem::take(&mut tr.text)),
                                        });
                                    }
                                    Err(e) => out.push(AgentEvent::TurnFailed {
                                        turn,
                                        accepted: tr.accepted,
                                        error: AgentError::new(error_kind(&e), format!("{e}")),
                                    }),
                                }
                                tr.turn = None;
                            }
                            for e in out {
                                ev.emit(e);
                            }
                        });
                    }
                    Cmd::Cancel => {
                        let _ =
                            conn.send_notification(v1::CancelNotification::new(session_id.clone()));
                    }
                    Cmd::Close => break,
                }
            }
            Ok(())
        })
        .await;

    // The connection is gone (closed, or the process exited).
    if let Some(tx) = ready_tx.lock().expect("lock").take() {
        let msg = match &result {
            Err(e) => format!("{e}"),
            Ok(()) => "agent exited during setup".into(),
        };
        let _ = tx.send(Err(AgentError::new(AgentErrorKind::Spawn, msg)));
        return;
    }
    if !started.load(std::sync::atomic::Ordering::SeqCst) {
        return; // setup failed and was reported through `open`
    }
    let failed = {
        let mut t = tracker.lock().expect("lock");
        t.turn.take().map(|turn| (turn, t.accepted))
    };
    let why = result.err().map(|e| Arc::from(format!("{e}")));
    if let Some((turn, accepted)) = failed {
        events.emit(AgentEvent::TurnFailed {
            turn,
            accepted,
            error: AgentError::new(
                AgentErrorKind::Transport,
                why.clone().unwrap_or_else(|| Arc::from("agent exited")),
            ),
        });
    }
    events.emit(AgentEvent::Exited { error: why });
}

#[allow(dead_code)]
fn _assert_connect_to<T: ConnectTo<Client>>() {}
