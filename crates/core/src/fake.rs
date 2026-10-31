//! A scripted ACP agent for tests. It speaks real ACP framing (it is built
//! with the same `agent-client-protocol` crate, agent side) and calls the chat
//! tools over real HTTP MCP with the token it was given in `session/new`.
//!
//! Runs in-process over an in-memory line transport ([`in_process`]) or as the
//! `fake-acp-agent` binary over stdio. A [`Script`] says what to do on each
//! prompt. Only the transport and the clock are fake in tests that use it.
//!
//! # Protocol versions
//!
//! By default the fake speaks ACP v1 only and answers a v2 `initialize` with
//! `protocolVersion: 1`, so a client that offers the v2 draft falls back. With
//! `"protocol": 2` in the script it also speaks the **v2 draft**
//! (`unstable_protocol_v2`): a v2 `initialize` selects v2, a v1 `initialize`
//! still gets v1. On v2 a turn is:
//!
//! - the `session/prompt` response straight away, carrying the accepted user
//!   message's `messageId` (v2 acknowledges acceptance there, not at the end),
//!   and a `user_message` update with that id and the prompt;
//! - `state_update: running`, then the steps;
//! - text as `agent_message_chunk`s keyed by an agent `messageId`; a tool call
//!   starts a new agent message;
//! - a tool call as a keyed `tool_call_update` (`in_progress`, with its title),
//!   upserted to `completed` (status only) once the agent has started its next
//!   message, so a client has to treat it as an update to a call it already
//!   knows, not a new one;
//! - a permission request inside `state_update: requires_action`, then
//!   `running` again;
//! - `state_update: idle` with the stop reason (`end_turn`, `cancelled`,
//!   `refusal`, `max_tokens`, or `error` for a scripted error after the turn
//!   was accepted).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agent_client_protocol::{
    self as acp, Agent, Client, ConnectTo, DynConnectTo, Lines, Responder, V2ConnectionTo,
    schema::{ProtocolVersion, v1, v2},
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Notify, watch};

/// What the agent does, prompt by prompt.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Script {
    /// Turn `i` answers the `i`-th prompt; the last one repeats.
    #[serde(default)]
    pub turns: Vec<Turn>,
    /// Fail `session/new` with this message (e.g. "Authentication required").
    #[serde(default)]
    pub fail_session_new: Option<String>,
    /// Advertise HTTP MCP (`mcpCapabilities.http` on v1,
    /// `capabilities.session.mcp.http` on v2; default true).
    #[serde(default = "yes")]
    pub mcp_http: bool,
    /// The highest ACP version spoken: 1 (default, v1 only) or 2 (also the v2
    /// draft; a v1 `initialize` still gets v1). See the module docs.
    #[serde(default)]
    pub protocol: Option<u16>,
    /// v1: offer a model selector (`configOptions`, category `model`) with
    /// these `[id, name]` pairs, the first one current. `session/set_config_option`
    /// and `session/close` are recorded in [`Observed`].
    #[serde(default)]
    pub models: Option<Vec<(String, String)>>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Turn {
    #[serde(default)]
    pub steps: Vec<Step>,
    /// `end_turn` (default), `cancelled`, `refusal`, `max_tokens`.
    #[serde(default)]
    pub stop: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// An `agent_message_chunk`.
    Text(String),
    /// v2 only (ignored on v1): an `agent_message` snapshot of the current
    /// agent message. Its content replaces whatever the chunks with the same
    /// `messageId` said, so a client must not append it.
    AgentMessage(String),
    /// A tool call (activity; also starts a new final message).
    ToolCall(String),
    /// Call a chat tool over HTTP MCP. In string args, `$TRIGGER` is the first
    /// owed message id from the prompt and `$N` the prompt number.
    Mcp { tool: String, args: Value },
    /// Ask for permission to run something (no tool kind given).
    Permission(String),
    /// Ask for permission for a tool call of this ACP kind (`read`,
    /// `search`, `edit`, `execute`, ...).
    PermissionKind { title: String, kind: String },
    /// Wait until the test opens this gate (in-process only; no-op otherwise).
    Gate(String),
    /// Real-time sleep.
    SleepMs(u64),
    /// Fail the turn. v1: a JSON-RPC error answers the prompt. v2: as the
    /// first step, a JSON-RPC error answers the prompt (not accepted);
    /// later, the turn ends `idle` with an `error` stop reason.
    Error(String),
    /// Exit (binary) or drop the connection (in-process) without answering.
    Exit,
}

/// Test-controlled gates for [`Step::Gate`].
#[derive(Default)]
pub struct Gates {
    gates: Mutex<HashMap<String, Arc<Notify>>>,
    opened: Mutex<Vec<String>>,
}

impl Gates {
    fn get(&self, name: &str) -> Arc<Notify> {
        self.gates
            .lock()
            .expect("lock")
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    pub fn open(&self, name: &str) {
        self.opened.lock().expect("lock").push(name.to_string());
        self.get(name).notify_one();
    }

    async fn wait(&self, name: &str) {
        if self.opened.lock().expect("lock").iter().any(|g| g == name) {
            return;
        }
        self.get(name).notified().await;
    }
}

/// What the fake saw, for assertions.
#[derive(Clone, Default, Debug)]
pub struct Observed {
    pub prompts: Vec<String>,
    pub sessions: Vec<Value>,
    pub permission_outcomes: Vec<Value>,
    pub tool_results: Vec<Result<Value, String>>,
    pub cancels: usize,
    /// The protocol version of each `initialize` the fake answered.
    pub protocols: Vec<u16>,
    /// `(configId, value)` of each `session/set_config_option`.
    pub config_sets: Vec<(String, String)>,
    /// Sessions closed with `session/close`.
    pub closed: Vec<String>,
}

/// The chat MCP endpoint (url, bearer token) from the latest `session/new`.
type McpEndpoint = Arc<Mutex<Option<(String, String)>>>;

#[derive(Clone)]
pub struct FakeAgent {
    pub script: Arc<Script>,
    pub gates: Arc<Gates>,
    pub observed: Arc<Mutex<Observed>>,
    /// Signalled by `Step::Exit` in-process.
    pub kill: Arc<Notify>,
    pub is_binary: bool,
    /// The session of the latest `session/cancel`; a turn in flight on that
    /// session ends with `cancelled`.
    cancel: Arc<watch::Sender<Option<String>>>,
}

impl FakeAgent {
    pub fn new(script: Script) -> Self {
        FakeAgent {
            script: Arc::new(script),
            gates: Arc::default(),
            observed: Arc::default(),
            kill: Arc::default(),
            is_binary: false,
            cancel: Arc::new(watch::channel(None).0),
        }
    }

    fn turn(&self, n: usize) -> Turn {
        self.script
            .turns
            .get(n)
            .or(self.script.turns.last())
            .cloned()
            .unwrap_or_default()
    }

    /// Records a prompt; returns its number.
    fn record_prompt(&self, text: &str) -> usize {
        let mut o = self.observed.lock().expect("lock");
        o.prompts.push(text.to_string());
        o.prompts.len() - 1
    }

    /// Records a `session/new` and keeps its HTTP chat MCP endpoint; returns
    /// the new session id, or the scripted failure.
    fn record_session(
        &self,
        req: Value,
        http: Option<(String, String)>,
        mcp: &McpEndpoint,
    ) -> Result<String, String> {
        let mut o = self.observed.lock().expect("lock");
        o.sessions.push(req);
        if let Some(msg) = &self.script.fail_session_new {
            return Err(msg.clone());
        }
        if let Some(endpoint) = http {
            *mcp.lock().expect("lock") = Some(endpoint);
        }
        Ok(format!("fake-session-{}", o.sessions.len()))
    }

    /// The scripted model selector, `current` (else the first) selected.
    fn model_options(&self, current: Option<&str>) -> Option<Vec<v1::SessionConfigOption>> {
        let models = self.script.models.as_ref()?;
        let current = current
            .map(str::to_string)
            .or_else(|| models.first().map(|m| m.0.clone()))?;
        let options: Vec<_> = models
            .iter()
            .map(|(id, name)| v1::SessionConfigSelectOption::new(id.clone(), name.clone()))
            .collect();
        Some(vec![
            v1::SessionConfigOption::select("model", "Model", current, options)
                .category(v1::SessionConfigOptionCategory::Model),
        ])
    }

    fn bearer(value: &str) -> String {
        value.trim_start_matches("Bearer ").to_string()
    }

    /// The agent side as a connectable component.
    pub fn component(self) -> DynConnectTo<Client> {
        let mcp: McpEndpoint = Arc::default();
        if self.script.protocol == Some(2) {
            DynConnectTo::new(
                Agent
                    .protocol_router()
                    .with_v1(self.clone().v1_component(mcp.clone()))
                    .with_v2(self.v2_component(mcp)),
            )
        } else {
            DynConnectTo::new(self.v1_component(mcp))
        }
    }

    fn v1_component(self, mcp: McpEndpoint) -> impl ConnectTo<Client> {
        let (me1, me2, me3, me4) = (self.clone(), self.clone(), self.clone(), self);
        Agent
            .builder()
            .name("fake-acp-agent")
            .on_receive_request(
                async move |_req: v1::InitializeRequest,
                            responder: Responder<v1::InitializeResponse>,
                            _cx| {
                    me1.observed.lock().expect("lock").protocols.push(1);
                    let mut caps = v1::AgentCapabilities::new();
                    caps.mcp_capabilities = v1::McpCapabilities::new().http(me1.script.mcp_http);
                    responder.respond(
                        v1::InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(caps)
                            .agent_info(v1::Implementation::new("fake-acp-agent", "0.0.0")),
                    )
                },
                acp::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let mcp = mcp.clone();
                    async move |req: v1::NewSessionRequest,
                                responder: Responder<v1::NewSessionResponse>,
                                _cx| {
                        let http = req.mcp_servers.iter().find_map(|server| match server {
                            v1::McpServer::Http(h) => Some((
                                h.url.clone(),
                                h.headers
                                    .iter()
                                    .find(|x| x.name.eq_ignore_ascii_case("authorization"))
                                    .map(|x| FakeAgent::bearer(&x.value))
                                    .unwrap_or_default(),
                            )),
                            _ => None,
                        });
                        let json = serde_json::to_value(&req).unwrap_or_default();
                        match me2.record_session(json, http, &mcp) {
                            Ok(id) => responder.respond(
                                v1::NewSessionResponse::new(id)
                                    .config_options(me2.model_options(None)),
                            ),
                            Err(msg) => responder.respond_with_error(acp::Error::new(-32000, msg)),
                        }
                    }
                },
                acp::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let me = me3.clone();
                    async move |req: v1::SetSessionConfigOptionRequest,
                                responder: Responder<v1::SetSessionConfigOptionResponse>,
                                _cx| {
                        let value = match &req.value {
                            v1::SessionConfigOptionValue::ValueId { value } => value.0.to_string(),
                            other => format!("{other:?}"),
                        };
                        me.observed
                            .lock()
                            .expect("lock")
                            .config_sets
                            .push((req.config_id.0.to_string(), value.clone()));
                        let opts = me.model_options(Some(&value)).unwrap_or_default();
                        responder.respond(v1::SetSessionConfigOptionResponse::new(opts))
                    }
                },
                acp::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let me = me3.clone();
                    async move |req: v1::CloseSessionRequest,
                                responder: Responder<v1::CloseSessionResponse>,
                                _cx| {
                        me.observed
                            .lock()
                            .expect("lock")
                            .closed
                            .push(req.session_id.to_string());
                        responder.respond(v1::CloseSessionResponse::new())
                    }
                },
                acp::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v1::PromptRequest,
                            responder: Responder<v1::PromptResponse>,
                            cx| {
                    let text: String = req
                        .prompt
                        .iter()
                        .filter_map(|b| match b {
                            v1::ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect();
                    let n = me3.record_prompt(&text);
                    let turn = me3.turn(n);
                    let me = me3.clone();
                    let mcp = mcp.clone();
                    let wire = Wire::V1 {
                        cx,
                        session: req.session_id.clone(),
                        responder: Some(responder),
                    };
                    tokio::spawn(async move {
                        me.run_turn(turn, n, text, wire, mcp).await;
                    });
                    Ok(())
                },
                acp::on_receive_request!(),
            )
            .on_receive_notification(
                async move |n: v1::CancelNotification, _cx| {
                    me4.observed.lock().expect("lock").cancels += 1;
                    me4.cancel.send_replace(Some(n.session_id.to_string()));
                    Ok(())
                },
                acp::on_receive_notification!(),
            )
    }

    fn v2_component(self, mcp: McpEndpoint) -> impl ConnectTo<Client> {
        let (me1, me2, me3, me4) = (self.clone(), self.clone(), self.clone(), self);
        Agent
            .v2()
            .name("fake-acp-agent")
            .on_receive_request(
                async move |_req: v2::InitializeRequest,
                            responder: Responder<v2::InitializeResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    me1.observed.lock().expect("lock").protocols.push(2);
                    let mut mcp_caps = v2::McpCapabilities::new();
                    if me1.script.mcp_http {
                        mcp_caps = mcp_caps.http(v2::McpHttpCapabilities::new());
                    }
                    responder.respond(
                        v2::InitializeResponse::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("fake-acp-agent", "0.0.0"),
                        )
                        .capabilities(
                            v2::AgentCapabilities::new()
                                .session(v2::SessionCapabilities::new().mcp(mcp_caps)),
                        ),
                    )
                },
                acp::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let mcp = mcp.clone();
                    async move |req: v2::NewSessionRequest,
                                responder: Responder<v2::NewSessionResponse>,
                                cx: V2ConnectionTo<Client>| {
                        let http = req.mcp_servers.iter().find_map(|server| match server {
                            v2::McpServer::Http(h) => Some((
                                h.url.clone(),
                                h.headers
                                    .iter()
                                    .find(|x| x.name.eq_ignore_ascii_case("authorization"))
                                    .map(|x| FakeAgent::bearer(&x.value))
                                    .unwrap_or_default(),
                            )),
                            _ => None,
                        });
                        let json = serde_json::to_value(&req).unwrap_or_default();
                        match me2.record_session(json, http, &mcp) {
                            Ok(id) => {
                                let session = v2::SessionId::new(id);
                                responder.respond(v2::NewSessionResponse::new(session.clone()))?;
                                // A fresh session reports that it is ready.
                                cx.send_notification(v2::UpdateSessionNotification::new(
                                    session,
                                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                        v2::IdleStateUpdate::new(),
                                    )),
                                ))
                            }
                            Err(msg) => responder.respond_with_error(acp::Error::new(-32000, msg)),
                        }
                    }
                },
                acp::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::PromptRequest,
                            responder: Responder<v2::PromptResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let text: String = req
                        .prompt
                        .iter()
                        .filter_map(|b| match b {
                            v2::ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect();
                    let n = me3.record_prompt(&text);
                    let turn = me3.turn(n);
                    // An error before anything else: the prompt is not accepted.
                    if let Some(Step::Error(msg)) = turn.steps.first() {
                        return responder.respond_with_error(acp::Error::new(-32603, msg.clone()));
                    }
                    // v2 acknowledges acceptance in the prompt response, then
                    // echoes the accepted prompt as the keyed user message.
                    let user = v2::MessageId::new(format!("user-{n}"));
                    responder.respond(v2::PromptResponse::new(user.clone()))?;
                    cx.send_notification(v2::UpdateSessionNotification::new(
                        req.session_id.clone(),
                        v2::SessionUpdate::UserMessage(
                            v2::UserMessage::new(user).content(req.prompt.clone()),
                        ),
                    ))?;
                    let me = me3.clone();
                    let mcp = mcp.clone();
                    let wire = Wire::V2 {
                        cx,
                        session: req.session_id.clone(),
                        message: 0,
                        open_call: None,
                    };
                    tokio::spawn(async move {
                        me.run_turn(turn, n, text, wire, mcp).await;
                    });
                    Ok(())
                },
                acp::on_receive_request!(),
            )
            .on_receive_notification(
                async move |n: v2::CancelSessionNotification, _cx: V2ConnectionTo<Client>| {
                    me4.observed.lock().expect("lock").cancels += 1;
                    me4.cancel.send_replace(Some(n.session_id.to_string()));
                    Ok(())
                },
                acp::on_receive_notification!(),
            )
    }

    async fn run_turn(
        self,
        turn: Turn,
        n: usize,
        prompt: String,
        mut wire: Wire,
        mcp: McpEndpoint,
    ) {
        // Subscribe before the first step so a cancel that lands at any point
        // in the turn is seen.
        let mut cancels = self.cancel.subscribe();
        cancels.mark_unchanged();
        let mine = wire.session();
        let cancelled = async move {
            while cancels.changed().await.is_ok() {
                if cancels.borrow_and_update().as_deref() == Some(mine.as_str()) {
                    return;
                }
            }
            std::future::pending::<()>().await
        };
        wire.started();
        let ending = tokio::select! {
            e = self.clone().steps(turn.steps, n, &prompt, &mut wire, mcp) => e,
            () = cancelled => Ending::Stop(Stop::Cancelled),
        };
        let stop = match ending {
            Ending::Stop(stop) => stop,
            Ending::Error(msg) => return wire.fail(msg),
            Ending::Dropped => return,
            Ending::Done => match turn.stop.as_deref() {
                Some("cancelled") => Stop::Cancelled,
                Some("refusal") => Stop::Refusal,
                Some("max_tokens") => Stop::MaxTokens,
                _ => Stop::EndTurn,
            },
        };
        wire.end(stop);
    }

    async fn steps(
        self,
        steps: Vec<Step>,
        n: usize,
        prompt: &str,
        wire: &mut Wire,
        mcp: McpEndpoint,
    ) -> Ending {
        let trigger = owed_trigger(prompt);
        for step in steps {
            match step {
                Step::Text(t) => wire.text(n, t),
                Step::AgentMessage(t) => wire.agent_message(n, t),
                Step::ToolCall(title) => wire.tool_call(n, title),
                Step::Mcp { tool, args } => {
                    let args = substitute(args, trigger.as_deref(), n);
                    let endpoint = mcp.lock().expect("lock").clone();
                    let result = match endpoint {
                        Some((url, token)) => tokio::task::spawn_blocking(move || {
                            crate::mcp::call_tool_http(&url, &token, &tool, args)
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string())),
                        None => Err("no chat MCP server was injected".to_string()),
                    };
                    self.observed
                        .lock()
                        .expect("lock")
                        .tool_results
                        .push(result);
                }
                Step::Permission(_) | Step::PermissionKind { .. } => {
                    let (title, kind) = match step {
                        Step::PermissionKind { title, kind } => (title, Some(kind)),
                        Step::Permission(title) => (title, None),
                        _ => unreachable!(),
                    };
                    let outcome = wire.permission(n, title, kind).await;
                    self.observed
                        .lock()
                        .expect("lock")
                        .permission_outcomes
                        .push(outcome);
                }
                Step::Gate(name) => {
                    if !self.is_binary {
                        self.gates.wait(&name).await;
                    }
                }
                Step::SleepMs(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
                Step::Error(msg) => return Ending::Error(msg),
                Step::Exit => {
                    if self.is_binary {
                        std::process::exit(3);
                    }
                    // Wakes the running connection only; no permit is stored, so
                    // the next run of the agent starts clean.
                    self.kill.notify_waiters();
                    return Ending::Dropped;
                }
            }
        }
        Ending::Done
    }
}

/// How a turn's steps ended.
enum Ending {
    /// Ran every step; answer with the turn's scripted stop reason.
    Done,
    Stop(Stop),
    Error(String),
    /// Exited or dropped the connection without answering.
    Dropped,
}

/// A stop reason, either protocol version.
#[derive(Clone, Copy)]
enum Stop {
    EndTurn,
    Cancelled,
    Refusal,
    MaxTokens,
}

/// One turn's way back to the client.
enum Wire {
    V1 {
        cx: acp::ConnectionTo<Client>,
        session: v1::SessionId,
        /// Answered when the turn ends (v1 prompts run to the end of the turn).
        responder: Option<Responder<v1::PromptResponse>>,
    },
    V2 {
        cx: V2ConnectionTo<Client>,
        session: v2::SessionId,
        /// The current agent message; a tool call starts the next one.
        message: usize,
        /// A tool call still `in_progress`, upserted to `completed` once the
        /// agent has started its next message.
        open_call: Option<v2::ToolCallId>,
    },
}

impl Wire {
    fn session(&self) -> String {
        match self {
            Wire::V1 { session, .. } => session.to_string(),
            Wire::V2 { session, .. } => session.to_string(),
        }
    }

    fn v2_update(cx: &V2ConnectionTo<Client>, session: &v2::SessionId, update: v2::SessionUpdate) {
        let _ = cx.send_notification(v2::UpdateSessionNotification::new(session.clone(), update));
    }

    fn message_id(n: usize, message: usize) -> v2::MessageId {
        v2::MessageId::new(format!("agent-{n}-{message}"))
    }

    /// The turn is running (v2 says so; v1 has no turn state).
    fn started(&mut self) {
        if let Wire::V2 { cx, session, .. } = self {
            Self::v2_update(
                cx,
                session,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                    v2::RunningStateUpdate::new(),
                )),
            );
        }
    }

    fn text(&mut self, n: usize, t: String) {
        match self {
            Wire::V1 { cx, session, .. } => {
                let _ = cx.send_notification(v1::SessionNotification::new(
                    session.clone(),
                    v1::SessionUpdate::AgentMessageChunk(v1::ContentChunk::new(
                        v1::ContentBlock::Text(v1::TextContent::new(t)),
                    )),
                ));
            }
            Wire::V2 {
                cx,
                session,
                message,
                open_call,
            } => {
                Self::v2_update(
                    cx,
                    session,
                    v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                        v2::ContentBlock::Text(v2::TextContent::new(t)),
                        Self::message_id(n, *message),
                    )),
                );
                if let Some(call) = open_call.take() {
                    Self::v2_update(
                        cx,
                        session,
                        v2::SessionUpdate::ToolCallUpdate(
                            v2::ToolCallUpdate::new(call).status(v2::ToolCallStatus::Completed),
                        ),
                    );
                }
            }
        }
    }

    fn agent_message(&mut self, n: usize, t: String) {
        if let Wire::V2 {
            cx,
            session,
            message,
            ..
        } = self
        {
            Self::v2_update(
                cx,
                session,
                v2::SessionUpdate::AgentMessage(
                    v2::AgentMessage::new(Self::message_id(n, *message))
                        .content(vec![v2::ContentBlock::Text(v2::TextContent::new(t))]),
                ),
            );
        }
    }

    fn tool_call(&mut self, n: usize, title: String) {
        match self {
            Wire::V1 { cx, session, .. } => {
                let _ = cx.send_notification(v1::SessionNotification::new(
                    session.clone(),
                    v1::SessionUpdate::ToolCall(v1::ToolCall::new(
                        v1::ToolCallId::new(format!("call-{n}")),
                        title,
                    )),
                ));
            }
            Wire::V2 {
                cx,
                session,
                message,
                open_call,
            } => {
                *message += 1;
                let id = v2::ToolCallId::new(format!("call-{n}-{message}"));
                Self::v2_update(
                    cx,
                    session,
                    v2::SessionUpdate::ToolCallUpdate(
                        v2::ToolCallUpdate::new(id.clone())
                            .title(title)
                            .status(v2::ToolCallStatus::InProgress),
                    ),
                );
                *open_call = Some(id);
            }
        }
    }

    /// Asks for permission; returns the client's answer (or the error) as JSON.
    async fn permission(&mut self, n: usize, title: String, kind: Option<String>) -> Value {
        match self {
            Wire::V1 { cx, session, .. } => {
                let mut fields = v1::ToolCallUpdateFields::new().title(title);
                if let Some(k) = kind.and_then(|k| serde_json::from_value(Value::String(k)).ok()) {
                    fields = fields.kind(Some(k));
                }
                let call =
                    v1::ToolCallUpdate::new(v1::ToolCallId::new(format!("perm-{n}")), fields);
                let req = v1::RequestPermissionRequest::new(
                    session.clone(),
                    call,
                    vec![
                        v1::PermissionOption::new(
                            "allow",
                            "Allow",
                            v1::PermissionOptionKind::AllowOnce,
                        ),
                        v1::PermissionOption::new(
                            "deny",
                            "Deny",
                            v1::PermissionOptionKind::RejectOnce,
                        ),
                    ],
                );
                let outcome = cx.send_request(req).block_task().await;
                outcome
                    .map(|o| serde_json::to_value(o).unwrap_or_default())
                    .unwrap_or_else(|e| Value::String(e.to_string()))
            }
            Wire::V2 { cx, session, .. } => {
                Self::v2_update(
                    cx,
                    session,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::RequiresAction(
                        v2::RequiresActionStateUpdate::new(),
                    )),
                );
                let mut req = v2::RequestPermissionRequest::new(
                    session.clone(),
                    title,
                    vec![
                        v2::PermissionOption::new(
                            "allow",
                            "Allow",
                            v2::PermissionOptionKind::AllowOnce,
                        ),
                        v2::PermissionOption::new(
                            "deny",
                            "Deny",
                            v2::PermissionOptionKind::RejectOnce,
                        ),
                    ],
                );
                if let Some(k) =
                    kind.and_then(|k| serde_json::from_value::<v2::ToolKind>(Value::String(k)).ok())
                {
                    req = req.subject(v2::RequestPermissionSubject::ToolCall(Box::new(
                        v2::ToolCallPermissionSubject::new(
                            v2::ToolCallUpdate::new(format!("perm-{n}")).kind(k),
                        ),
                    )));
                }
                let outcome = cx.send_request(req).block_task().await;
                Self::v2_update(
                    cx,
                    session,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                        v2::RunningStateUpdate::new(),
                    )),
                );
                outcome
                    .map(|o| serde_json::to_value(o).unwrap_or_default())
                    .unwrap_or_else(|e| Value::String(e.to_string()))
            }
        }
    }

    /// Ends the turn normally.
    fn end(self, stop: Stop) {
        match self {
            Wire::V1 { responder, .. } => {
                let stop = match stop {
                    Stop::EndTurn => v1::StopReason::EndTurn,
                    Stop::Cancelled => v1::StopReason::Cancelled,
                    Stop::Refusal => v1::StopReason::Refusal,
                    Stop::MaxTokens => v1::StopReason::MaxTokens,
                };
                if let Some(r) = responder {
                    let _ = r.respond(v1::PromptResponse::new(stop));
                }
            }
            Wire::V2 {
                cx,
                session,
                open_call,
                ..
            } => {
                if let Some(call) = open_call {
                    let status = match stop {
                        Stop::Cancelled => v2::ToolCallStatus::Cancelled,
                        _ => v2::ToolCallStatus::Completed,
                    };
                    Self::v2_update(
                        &cx,
                        &session,
                        v2::SessionUpdate::ToolCallUpdate(
                            v2::ToolCallUpdate::new(call).status(status),
                        ),
                    );
                }
                let stop = match stop {
                    Stop::EndTurn => v2::StopReason::EndTurn,
                    Stop::Cancelled => v2::StopReason::Cancelled,
                    Stop::Refusal => v2::StopReason::Refusal,
                    Stop::MaxTokens => v2::StopReason::MaxTokens,
                };
                Self::v2_update(
                    &cx,
                    &session,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                        v2::IdleStateUpdate::new().stop_reason(stop),
                    )),
                );
            }
        }
    }

    /// Ends the turn with a scripted error.
    fn fail(self, msg: String) {
        match self {
            Wire::V1 { responder, .. } => {
                if let Some(r) = responder {
                    let _ = r.respond_with_error(acp::Error::new(-32603, msg));
                }
            }
            Wire::V2 { cx, session, .. } => Self::v2_update(
                &cx,
                &session,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                    v2::IdleStateUpdate::new().stop_reason(v2::StopReason::Error(
                        v2::ErrorStopReason::new().error(v2::Error::new(-32603, msg)),
                    )),
                )),
            ),
        }
    }
}

fn owed_trigger(prompt: &str) -> Option<String> {
    let rest = prompt.split("You owe a reply to id:").nth(1)?;
    Some(rest.chars().take_while(|c| c.is_ascii_digit()).collect())
}

fn substitute(v: Value, trigger: Option<&str>, n: usize) -> Value {
    match v {
        Value::String(s) if s == "$TRIGGER" => trigger
            .and_then(|t| t.parse::<u64>().ok())
            .map(Value::from)
            .unwrap_or(Value::Null),
        Value::String(s) => Value::String(s.replace("$N", &n.to_string())),
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (k, substitute(v, trigger, n)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| substitute(v, trigger, n)).collect()),
        other => other,
    }
}

/// An in-process connection to a fresh run of `agent`: an in-memory line
/// transport, with the agent on its own task so [`Step::Exit`] can drop it the
/// way a dying process would.
pub fn in_process(agent: FakeAgent) -> DynConnectTo<Client> {
    let (to_agent_tx, to_agent_rx) = futures::channel::mpsc::unbounded::<String>();
    let (to_client_tx, to_client_rx) = futures::channel::mpsc::unbounded::<String>();
    let (to_client, to_agent) = (to_client_tx.clone(), to_agent_tx.clone());
    let agent_side = Lines::new(
        to_client_tx.sink_map_err(std::io::Error::other),
        to_agent_rx.map(Ok::<_, std::io::Error>),
    );
    let client_side = Lines::new(
        to_agent_tx.sink_map_err(std::io::Error::other),
        to_client_rx.map(Ok::<_, std::io::Error>),
    );
    let kill = agent.kill.clone();
    let component = agent.component();
    tokio::spawn(async move {
        tokio::select! {
            _ = ConnectTo::<Client>::connect_to(component, agent_side) => {}
            _ = kill.notified() => {}
        }
        // Like a dying process: both pipes close, so the client sees EOF.
        to_client.close_channel();
        to_agent.close_channel();
    });
    DynConnectTo::new(client_side)
}

/// Entry point for the `fake-acp-agent` binary: script from the JSON file named
/// by `FAKE_ACP_SCRIPT` (or none), stdio transport.
pub async fn binary_main() -> Result<(), acp::Error> {
    let script = std::env::var("FAKE_ACP_SCRIPT")
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Script>(&s).ok())
        .unwrap_or_default();
    let mut agent = FakeAgent::new(script);
    agent.is_binary = true;
    let component = agent.component();
    ConnectTo::<Client>::connect_to(component, acp::Stdio::new()).await
}
