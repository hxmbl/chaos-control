//! Native Codex adapter: the Codex app-server's JSON-RPC over stdio
//! (`codex app-server`).
//!
//! ACP reaches Codex only through the `codex-acp` adapter, which lags the
//! app-server and hides its model list. The app-server is what Codex's own
//! IDE extensions and T3 Code (github.com/pingdotgg/t3code, MIT) drive, so
//! this adapter speaks it directly, behind the same [`AgentAdapter`] /
//! [`AgentSession`] boundary as ACP. The runtime never knows the difference.
//! The method names and payload shapes follow T3 Code's `CodexAdapterV2.ts`
//! and `CodexProvider.ts` (studied, not copied: this is a fresh Rust client).
//!
//! # Wire
//!
//! JSON-RPC 2.0 framing, one JSON object per line, with the `"jsonrpc"`
//! member left out (the app-server omits it; we accept it if present).
//!
//! - `initialize {clientInfo}` then the `initialized` notification.
//! - `model/list {cursor?}` (paged) for the member's model list.
//! - `thread/start {cwd, model?, config}`: `config` carries our chat MCP
//!   server as a `mcp_servers.chaos-control` override (streamable HTTP with
//!   a bearer header, or the stdio bridge), so the user's own MCP servers
//!   stay as they are. Nothing else is overridden: Chaos Control never
//!   changes an agent's approval or sandbox mode.
//! - `turn/start {threadId, input: [{type: "text", text}]}` per batch;
//!   `turn/interrupt {threadId, turnId}` to cancel.
//! - Notifications read: `turn/started`, `item/started` (activity),
//!   `item/completed` (`agentMessage` text), `turn/completed`.
//! - Server requests answered: `item/commandExecution/requestApproval` and
//!   `item/fileChange/requestApproval` go through the permission gate and
//!   get `{decision: "accept"}` or `{decision: "decline"}` (decline lets the
//!   turn continue; Codex's `cancel` would end it). Anything else is
//!   answered with a JSON-RPC "method not found" error.
//!
//! # Acceptance point
//!
//! The `turn/start` response (it returns the created turn) or an earlier
//! `turn/started` for it, whichever arrives first. An error response before
//! either is "not accepted" (retryable). The turn ends at `turn/completed`:
//! `completed` and `interrupted` end it normally (`interrupted` as
//! `Cancelled`), `failed` fails it after acceptance.
//!
//! # Final text
//!
//! The text of the last `agentMessage` item after the last tool-like item,
//! like the ACP adapter: a fallback-reply candidate, never posted on its own.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
};

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};

use crate::acp::{ChatToolsPreference, auth_like, resolve_command};
use crate::adapter::*;
use crate::harness::ModelInfo;
use crate::permission::{PermissionAsk, PermissionGate, PermissionVerdict};
use crate::types::BatchId;

/// The name our chat MCP server is registered under inside Codex.
pub const MCP_NAME: &str = "chaos-control";

/// A byte pipe to one app-server process (or an in-process fake).
pub struct CodexPipe {
    pub write: Box<dyn AsyncWrite + Send + Unpin>,
    pub read: Box<dyn AsyncRead + Send + Unpin>,
    /// Killed when the session closes.
    pub child: Option<tokio::process::Child>,
}

/// How to reach the app-server.
pub trait CodexTransport: Send + Sync {
    fn describe(&self) -> String;
    fn probe(&self) -> Probe;
    fn connect(&self) -> Result<CodexPipe, AgentError>;
}

/// Spawns `codex app-server` (or whatever command the preset names).
pub struct CodexSpawn {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl CodexTransport for CodexSpawn {
    fn describe(&self) -> String {
        format!(
            "codex app-server: {} {}",
            self.command.display(),
            self.args.join(" ")
        )
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

    fn connect(&self) -> Result<CodexPipe, AgentError> {
        let mut cmd = tokio::process::Command::new(&self.command);
        cmd.args(&self.args)
            .envs(self.env.iter().cloned())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| AgentError::new(AgentErrorKind::Spawn, e.to_string()))?;
        let write = child.stdin.take().expect("piped stdin");
        let read = child.stdout.take().expect("piped stdout");
        Ok(CodexPipe {
            write: Box::new(write),
            read: Box::new(read),
            child: Some(child),
        })
    }
}

pub struct CodexAdapter {
    transport: Arc<dyn CodexTransport>,
    model: Option<Arc<str>>,
    chat_tools: ChatToolsPreference,
}

impl CodexAdapter {
    pub fn new(transport: Arc<dyn CodexTransport>) -> Self {
        CodexAdapter {
            transport,
            model: None,
            chat_tools: ChatToolsPreference::Auto,
        }
    }

    /// Runs threads on `model` (Codex's own id); `None` uses Codex's default.
    pub fn model(mut self, model: Option<Arc<str>>) -> Self {
        self.model = model;
        self
    }

    pub fn chat_tools(mut self, pref: ChatToolsPreference) -> Self {
        self.chat_tools = pref;
        self
    }
}

impl AgentAdapter for CodexAdapter {
    fn describe(&self) -> String {
        self.transport.describe()
    }

    fn probe(&self) -> BoxFuture<'_, Probe> {
        Box::pin(async move { self.transport.probe() })
    }

    fn open(&self, req: OpenSession) -> BoxFuture<'_, Result<Arc<dyn AgentSession>, AgentError>> {
        Box::pin(async move {
            let pipe = self.transport.connect()?;
            open(pipe, req, self.model.clone(), self.chat_tools).await
        })
    }

    fn list_models(
        &self,
        _cwd: PathBuf,
    ) -> BoxFuture<'_, Option<Result<Vec<ModelInfo>, AgentError>>> {
        Box::pin(async move {
            let pipe = match self.transport.connect() {
                Ok(p) => p,
                Err(e) => return Some(Err(e)),
            };
            let (rpc, mut incoming, child) = Rpc::start(pipe);
            // Nothing should ask us anything here; refuse whatever does.
            let refuse = rpc.clone();
            tokio::spawn(async move {
                while let Some(m) = incoming.recv().await {
                    if let Incoming::Request { id, .. } = m {
                        refuse.respond_error(id, -32601, "not supported while listing models");
                    }
                }
            });
            let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                initialize(&rpc).await?;
                list_models(&rpc).await
            })
            .await
            .unwrap_or_else(|_| {
                Err(AgentError::new(
                    AgentErrorKind::Transport,
                    "no answer while listing models",
                ))
            });
            drop(child);
            Some(result)
        })
    }
}

// ---------- JSON-RPC client ----------

#[derive(Debug)]
enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

#[derive(Clone, Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn to_agent(&self, what: &str) -> AgentError {
        let kind = if auth_like(&self.message) {
            AgentErrorKind::Auth
        } else if self.code == TRANSPORT_GONE {
            AgentErrorKind::Transport
        } else {
            AgentErrorKind::Protocol
        };
        AgentError::new(kind, format!("{what}: {}", self.message))
    }
}

const TRANSPORT_GONE: i64 = -1;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

#[derive(Clone)]
struct Rpc {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: Arc<AtomicI64>,
}

/// Keeps the child process alive; dropping it kills the process.
struct ChildGuard(#[allow(dead_code)] Option<tokio::process::Child>);

impl Rpc {
    fn start(pipe: CodexPipe) -> (Rpc, mpsc::UnboundedReceiver<Incoming>, ChildGuard) {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Incoming>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let mut write = pipe.write;
        tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                if write.write_all(line.as_bytes()).await.is_err()
                    || write.write_all(b"\n").await.is_err()
                    || write.flush().await.is_err()
                {
                    break;
                }
            }
        });
        let pend = pending.clone();
        let mut lines = BufReader::new(pipe.read).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue; // not a frame (log noise)
                };
                let method = msg
                    .get("method")
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
                let id = msg.get("id").cloned().filter(|v| !v.is_null());
                match (method, id) {
                    (Some(method), Some(id)) => {
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        let _ = in_tx.send(Incoming::Request { id, method, params });
                    }
                    (Some(method), None) => {
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        let _ = in_tx.send(Incoming::Notification { method, params });
                    }
                    (None, Some(id)) => {
                        let Some(id) = id.as_i64() else { continue };
                        let Some(tx) = pend.lock().expect("lock").remove(&id) else {
                            continue;
                        };
                        let result = match msg.get("error") {
                            Some(e) => Err(RpcError {
                                code: e.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                                message: e
                                    .get("message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("error")
                                    .to_string(),
                            }),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(result);
                    }
                    (None, None) => {}
                }
            }
            // EOF: fail everything still waiting; dropping `in_tx` closes
            // the incoming stream, which is how the session sees the exit.
            for (_, tx) in pend.lock().expect("lock").drain() {
                let _ = tx.send(Err(RpcError {
                    code: TRANSPORT_GONE,
                    message: "app-server exited".into(),
                }));
            }
        });
        (
            Rpc {
                out: out_tx,
                pending,
                next: Arc::new(AtomicI64::new(1)),
            },
            in_rx,
            ChildGuard(pipe.child),
        )
    }

    /// Writes the request now; the returned future waits for its answer.
    fn request(
        &self,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value, RpcError>> + Send + 'static {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("lock").insert(id, tx);
        let sent = self
            .out
            .send(json!({ "id": id, "method": method, "params": params }).to_string());
        async move {
            if sent.is_err() {
                return Err(RpcError {
                    code: TRANSPORT_GONE,
                    message: "app-server is gone".into(),
                });
            }
            rx.await.unwrap_or_else(|_| {
                Err(RpcError {
                    code: TRANSPORT_GONE,
                    message: "app-server exited".into(),
                })
            })
        }
    }

    fn notify(&self, method: &str, params: Option<Value>) {
        let msg = match params {
            Some(p) => json!({ "method": method, "params": p }),
            None => json!({ "method": method }),
        };
        let _ = self.out.send(msg.to_string());
    }

    fn respond(&self, id: Value, result: Value) {
        let _ = self
            .out
            .send(json!({ "id": id, "result": result }).to_string());
    }

    fn respond_error(&self, id: Value, code: i64, message: &str) {
        let _ = self
            .out
            .send(json!({ "id": id, "error": { "code": code, "message": message } }).to_string());
    }
}

async fn initialize(rpc: &Rpc) -> Result<Value, AgentError> {
    let init = rpc
        .request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "chaos-control",
                    "title": "Chaos Control",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )
        .await
        .map_err(|e| e.to_agent("initialize"))?;
    rpc.notify("initialized", None);
    Ok(init)
}

/// Every page of `model/list`, hidden models left out.
async fn list_models(rpc: &Rpc) -> Result<Vec<ModelInfo>, AgentError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..20 {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let page = rpc
            .request("model/list", params)
            .await
            .map_err(|e| e.to_agent("model/list"))?;
        for m in page
            .get("data")
            .and_then(|d| d.as_array())
            .into_iter()
            .flatten()
        {
            if m.get("hidden").and_then(|h| h.as_bool()) == Some(true) {
                continue;
            }
            let Some(id) = m
                .get("model")
                .or_else(|| m.get("id"))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let name = m.get("displayName").and_then(|v| v.as_str()).unwrap_or(id);
            out.push(ModelInfo::new(Some(id), name));
        }
        cursor = page
            .get("nextCursor")
            .and_then(|c| c.as_str())
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok(out)
}

/// The `mcp_servers.<name>` override that hands Codex our chat tools.
fn mcp_config(pref: ChatToolsPreference, ep: &ChatToolsEndpoint) -> (Value, ChatToolsTransport) {
    if let (ChatToolsPreference::StdioBridge, Some(bridge)) = (pref, &ep.bridge) {
        return (
            json!({
                "command": bridge.command.to_string_lossy(),
                "args": bridge.args,
                "env": { "CHAOS_MCP_URL": ep.url, "CHAOS_MCP_TOKEN": ep.token },
            }),
            ChatToolsTransport::StdioBridge,
        );
    }
    (
        json!({
            "url": ep.url,
            "http_headers": { "Authorization": format!("Bearer {}", ep.token) },
        }),
        ChatToolsTransport::Http,
    )
}

// ---------- session ----------

enum Cmd {
    Prompt {
        turn: BatchId,
        text: Arc<str>,
        done: oneshot::Sender<Result<(), AgentError>>,
    },
    Cancel,
    Close(oneshot::Sender<()>),
}

#[derive(Default)]
struct Turn {
    turn: Option<BatchId>,
    /// Codex's turn id, once known.
    native: Option<String>,
    accepted: bool,
    text: String,
}

impl Turn {
    fn accept(&mut self) -> Option<AgentEvent> {
        match self.turn {
            Some(turn) if !self.accepted => {
                self.accepted = true;
                Some(AgentEvent::Accepted { turn })
            }
            _ => None,
        }
    }
}

struct CodexSession {
    info: SessionInfo,
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl AgentSession for CodexSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    fn deliver(&self, turn: BatchId, prompt: Arc<str>) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            let (done, wait) = oneshot::channel();
            self.cmd
                .send(Cmd::Prompt {
                    turn,
                    text: prompt,
                    done,
                })
                .map_err(|_| AgentError::new(AgentErrorKind::Transport, "session closed"))?;
            wait.await
                .map_err(|_| AgentError::new(AgentErrorKind::Transport, "session closed"))?
        })
    }

    fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            self.cmd
                .send(Cmd::Cancel)
                .map_err(|_| AgentError::new(AgentErrorKind::Transport, "session closed"))
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            if self.cmd.send(Cmd::Close(tx)).is_ok() {
                let _ = rx.await;
            }
        })
    }
}

async fn open(
    pipe: CodexPipe,
    req: OpenSession,
    model: Option<Arc<str>>,
    pref: ChatToolsPreference,
) -> Result<Arc<dyn AgentSession>, AgentError> {
    let (rpc, mut incoming, child) = Rpc::start(pipe);
    let init = initialize(&rpc).await?;
    // The model list is a nicety: a Codex too old for it still runs.
    let models = list_models(&rpc).await.unwrap_or_default();
    let use_model = model
        .as_ref()
        .filter(|m| models.is_empty() || models.iter().any(|x| x.id.as_deref() == Some(&***m)));
    let (mcp, chat_tools) = mcp_config(pref, &req.chat_tools);
    let mut params = json!({
        "cwd": req.cwd.to_string_lossy(),
        "config": { format!("mcp_servers.{MCP_NAME}"): mcp },
    });
    if let Some(m) = use_model {
        params["model"] = json!(m.as_ref());
    }
    let started = rpc
        .request("thread/start", params)
        .await
        .map_err(|e| e.to_agent("thread/start"))?;
    let thread = started
        .pointer("/thread/id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::new(AgentErrorKind::Protocol, "thread/start: no thread id"))?
        .to_string();
    let running = started
        .get("model")
        .and_then(|v| v.as_str())
        .map(Arc::from)
        .or_else(|| use_model.cloned());
    let applied = if model.is_some() && use_model.is_none() {
        Err("not in model/list".to_string())
    } else {
        Ok(())
    };
    let model_note = model_outcome(model.as_deref(), &models, applied);
    let info = SessionInfo {
        caps: Caps::INTERRUPT
            | match chat_tools {
                ChatToolsTransport::Http => Caps::CHAT_TOOLS_HTTP,
                _ => Caps::CHAT_TOOLS_STDIO,
            },
        protocol: Arc::from("codex/app-server"),
        native_ref: Arc::from(thread.as_str()),
        agent_name: init
            .get("userAgent")
            .and_then(|v| v.as_str())
            .map(Arc::from),
        unused_extensions: Vec::new(),
        chat_tools,
        model: running,
        models,
        model_note,
    };
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let events = req.events.clone();
    let gate = req.permissions.clone();
    tokio::spawn(async move {
        let _child = child;
        let state = Arc::new(Mutex::new(Turn::default()));
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(Cmd::Prompt { turn, text, done }) => {
                        *state.lock().expect("lock") = Turn { turn: Some(turn), ..Turn::default() };
                        let answer = rpc.request(
                            "turn/start",
                            json!({ "threadId": thread, "input": [{ "type": "text", "text": &*text }] }),
                        );
                        // Written to the transport: the turn is handed over.
                        let _ = done.send(Ok(()));
                        let (state, events) = (state.clone(), events.clone());
                        tokio::spawn(async move {
                            let result = answer.await;
                            let mut out = Vec::new();
                            {
                                let mut t = state.lock().expect("lock");
                                if t.turn != Some(turn) {
                                    return;
                                }
                                match result {
                                    Ok(v) => {
                                        if let Some(id) = v.pointer("/turn/id").and_then(|i| i.as_str()) {
                                            t.native.get_or_insert_with(|| id.to_string());
                                        }
                                        out.extend(t.accept());
                                    }
                                    Err(e) if !t.accepted => {
                                        t.turn = None;
                                        out.push(AgentEvent::TurnFailed {
                                            turn,
                                            accepted: false,
                                            error: e.to_agent("turn/start"),
                                        });
                                    }
                                    Err(_) => {} // turn/completed reports it
                                }
                            }
                            out.into_iter().for_each(|e| events.emit(e));
                        });
                    }
                    Some(Cmd::Cancel) => {
                        let native = state.lock().expect("lock").native.clone();
                        if let Some(turn_id) = native {
                            // Written now; its answer does not matter.
                            drop(rpc.request(
                                "turn/interrupt",
                                json!({ "threadId": thread, "turnId": turn_id }),
                            ));
                        }
                    }
                    Some(Cmd::Close(done)) => {
                        let _ = done.send(());
                        return; // drops the child: the process is killed
                    }
                    None => return,
                },
                msg = incoming.recv() => match msg {
                    Some(Incoming::Notification { method, params }) => {
                        for e in on_notification(&state, &method, &params) {
                            events.emit(e);
                        }
                    }
                    Some(Incoming::Request { id, method, params }) => {
                        on_request(&rpc, &state, &gate, id, &method, &params, &events);
                    }
                    None => {
                        // The app-server exited.
                        let failed = {
                            let mut t = state.lock().expect("lock");
                            t.turn.take().map(|turn| (turn, t.accepted))
                        };
                        if let Some((turn, accepted)) = failed {
                            events.emit(AgentEvent::TurnFailed {
                                turn,
                                accepted,
                                error: AgentError::new(AgentErrorKind::Transport, "app-server exited"),
                            });
                        }
                        events.emit(AgentEvent::Exited { error: Some(Arc::from("app-server exited")) });
                        return;
                    }
                },
            }
        }
    });
    Ok(Arc::new(CodexSession { info, cmd: cmd_tx }))
}

/// Events for one notification. Pure apart from the turn state, so the
/// mapping is unit-tested without a process.
fn on_notification(state: &Mutex<Turn>, method: &str, p: &Value) -> Vec<AgentEvent> {
    let mut t = state.lock().expect("lock");
    let Some(turn) = t.turn else {
        return Vec::new();
    };
    let turn_id = p
        .pointer("/turn/id")
        .or_else(|| p.get("turnId"))
        .and_then(|v| v.as_str());
    // Ignore another turn's traffic once ours is known.
    if let (Some(id), Some(native)) = (turn_id, &t.native)
        && id != native
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    match method {
        "turn/started" => {
            if let Some(id) = turn_id {
                t.native.get_or_insert_with(|| id.to_string());
            }
            out.extend(t.accept());
        }
        "item/started" => {
            let item = p.get("item").unwrap_or(&Value::Null);
            let label = match item.get("type").and_then(|v| v.as_str()) {
                Some("commandExecution") => item
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| format!("Running {c}")),
                Some("mcpToolCall") => item
                    .get("tool")
                    .and_then(|c| c.as_str())
                    .map(|c| format!("Using {c}")),
                Some("fileChange") => Some("Editing files".to_string()),
                Some("webSearch") => Some("Searching the web".to_string()),
                _ => None,
            };
            if let Some(label) = label {
                // A tool ran: earlier text was working-out, not the answer.
                t.text.clear();
                out.extend(t.accept());
                out.push(AgentEvent::Activity {
                    label: Arc::from(label.chars().take(120).collect::<String>()),
                });
            }
        }
        "item/completed" => {
            let item = p.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(|v| v.as_str()) == Some("agentMessage") {
                out.extend(t.accept());
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    t.text = text.to_string();
                }
            }
        }
        "turn/completed" => {
            let status = p
                .pointer("/turn/status")
                .and_then(|v| v.as_str())
                .unwrap_or("completed");
            let text = Arc::from(std::mem::take(&mut t.text));
            match status {
                "failed" => {
                    let message = p
                        .pointer("/turn/error/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("turn failed");
                    let kind = if auth_like(message) {
                        AgentErrorKind::Auth
                    } else {
                        AgentErrorKind::Protocol
                    };
                    let accepted = t.accepted;
                    out.push(AgentEvent::TurnFailed {
                        turn,
                        accepted,
                        error: AgentError::new(kind, message),
                    });
                }
                "interrupted" => {
                    out.extend(t.accept());
                    out.push(AgentEvent::TurnEnded {
                        turn,
                        outcome: TurnOutcome::Cancelled,
                        final_text: text,
                    });
                }
                _ => {
                    out.extend(t.accept());
                    out.push(AgentEvent::TurnEnded {
                        turn,
                        outcome: TurnOutcome::Completed,
                        final_text: text,
                    });
                }
            }
            *t = Turn::default();
        }
        _ => {}
    }
    out
}

fn on_request(
    rpc: &Rpc,
    state: &Arc<Mutex<Turn>>,
    gate: &PermissionGate,
    id: Value,
    method: &str,
    p: &Value,
    events: &EventSink,
) {
    let title = match method {
        "item/commandExecution/requestApproval" => p
            .get("command")
            .and_then(|c| c.as_str())
            .or_else(|| p.get("reason").and_then(|c| c.as_str()))
            .unwrap_or("run a command")
            .to_string(),
        "item/fileChange/requestApproval" => match p.get("reason").and_then(|c| c.as_str()) {
            Some(r) => format!("Edit files: {r}"),
            None => "Edit files".to_string(),
        },
        _ => {
            rpc.respond_error(id, -32601, "not supported by Chaos Control");
            return;
        }
    };
    let (accepted, turn) = {
        let mut t = state.lock().expect("lock");
        (t.accept(), t.turn)
    };
    if let Some(e) = accepted {
        events.emit(e);
    }
    let ask = gate.ask(PermissionAsk {
        turn,
        title: Arc::from(title),
        detail: Some(Arc::from(p.to_string())),
    });
    let rpc = rpc.clone();
    tokio::spawn(async move {
        let decision = match ask.await {
            PermissionVerdict::Allow { .. } => "accept",
            PermissionVerdict::Sent { .. } | PermissionVerdict::Decline => "decline",
        };
        rpc.respond(id, json!({ "decision": decision }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_state() -> Mutex<Turn> {
        Mutex::new(Turn {
            turn: Some(BatchId(7)),
            ..Turn::default()
        })
    }

    fn kinds(events: &[AgentEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                AgentEvent::Accepted { .. } => "accepted",
                AgentEvent::Activity { .. } => "activity",
                AgentEvent::TurnEnded { .. } => "ended",
                AgentEvent::TurnFailed { .. } => "failed",
                AgentEvent::Exited { .. } => "exited",
            })
            .collect()
    }

    #[test]
    fn a_turn_accepts_once_and_keeps_the_last_message_after_tools() {
        let s = turn_state();
        let e = on_notification(&s, "turn/started", &json!({"turn": {"id": "t1"}}));
        assert_eq!(kinds(&e), ["accepted"]);
        let e = on_notification(
            &s,
            "item/completed",
            &json!({"turnId": "t1", "item": {"type": "agentMessage", "text": "Looking"}}),
        );
        assert!(e.is_empty());
        let e = on_notification(
            &s,
            "item/started",
            &json!({"turnId": "t1", "item": {"type": "commandExecution", "command": "cargo test"}}),
        );
        assert_eq!(kinds(&e), ["activity"]);
        on_notification(
            &s,
            "item/completed",
            &json!({"turnId": "t1", "item": {"type": "agentMessage", "text": "All green"}}),
        );
        // Another turn's traffic is ignored.
        assert!(on_notification(&s, "turn/completed", &json!({"turn": {"id": "zz"}})).is_empty());
        let e = on_notification(
            &s,
            "turn/completed",
            &json!({"turn": {"id": "t1", "status": "completed"}}),
        );
        match &e[..] {
            [
                AgentEvent::TurnEnded {
                    outcome,
                    final_text,
                    ..
                },
            ] => {
                assert_eq!(*outcome, TurnOutcome::Completed);
                assert_eq!(&**final_text, "All green");
            }
            other => panic!("{other:?}"),
        }
        assert!(s.lock().unwrap().turn.is_none());
    }

    #[test]
    fn interrupted_is_cancelled_and_failed_after_acceptance_is_a_failure() {
        let s = turn_state();
        let e = on_notification(
            &s,
            "turn/completed",
            &json!({"turn": {"id": "t1", "status": "interrupted"}}),
        );
        assert_eq!(kinds(&e), ["accepted", "ended"]);
        assert!(matches!(
            e[1],
            AgentEvent::TurnEnded {
                outcome: TurnOutcome::Cancelled,
                ..
            }
        ));

        let s = turn_state();
        on_notification(&s, "turn/started", &json!({"turn": {"id": "t2"}}));
        let e = on_notification(
            &s,
            "turn/completed",
            &json!({"turn": {"id": "t2", "status": "failed", "error": {"message": "Please log in to Codex"}}}),
        );
        match &e[..] {
            [
                AgentEvent::TurnFailed {
                    accepted, error, ..
                },
            ] => {
                assert!(*accepted);
                assert_eq!(error.kind, AgentErrorKind::Auth);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mcp_override_is_http_with_a_bearer_unless_forced_onto_the_bridge() {
        let ep = ChatToolsEndpoint {
            url: "http://127.0.0.1:9/mcp".into(),
            token: "tok".into(),
            bridge: Some(BridgeCommand {
                command: PathBuf::from("/bin/chaos-control"),
                args: vec!["mcp-bridge".into()],
            }),
        };
        let (v, t) = mcp_config(ChatToolsPreference::Auto, &ep);
        assert_eq!(t, ChatToolsTransport::Http);
        assert_eq!(v["http_headers"]["Authorization"], "Bearer tok");
        let (v, t) = mcp_config(ChatToolsPreference::StdioBridge, &ep);
        assert_eq!(t, ChatToolsTransport::StdioBridge);
        assert_eq!(v["env"]["CHAOS_MCP_TOKEN"], "tok");
        assert_eq!(v["args"][0], "mcp-bridge");
    }
}
