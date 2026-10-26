//! A scripted ACP agent for tests. It speaks real ACP framing (it is built
//! with the same `agent-client-protocol` crate, agent side) and calls the chat
//! tools over real HTTP MCP with the token it was given in `session/new`.
//!
//! Runs in-process over an in-memory line transport ([`in_process`]) or as the
//! `fake-acp-agent` binary over stdio. A [`Script`] says what to do on each
//! prompt. Only the transport and the clock are fake in tests that use it.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agent_client_protocol::{
    self as acp, Agent, Client, ConnectTo, DynConnectTo, Lines, Responder,
    schema::{ProtocolVersion, v1},
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Notify;

/// What the agent does, prompt by prompt.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Script {
    /// Turn `i` answers the `i`-th prompt; the last one repeats.
    #[serde(default)]
    pub turns: Vec<Turn>,
    /// Fail `session/new` with this message (e.g. "Authentication required").
    #[serde(default)]
    pub fail_session_new: Option<String>,
    /// Advertise `mcpCapabilities.http` (default true).
    #[serde(default = "yes")]
    pub mcp_http: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Turn {
    #[serde(default)]
    pub steps: Vec<Step>,
    /// `end_turn` (default), `cancelled`, `refusal`.
    #[serde(default)]
    pub stop: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// An `agent_message_chunk`.
    Text(String),
    /// A `tool_call` update (activity; also starts a new final message).
    ToolCall(String),
    /// Call a chat tool over HTTP MCP. In string args, `$TRIGGER` is the first
    /// owed message id from the prompt and `$N` the prompt number.
    Mcp { tool: String, args: Value },
    /// Ask for permission to run something.
    Permission(String),
    /// Wait until the test opens this gate (in-process only; no-op otherwise).
    Gate(String),
    /// Real-time sleep.
    SleepMs(u64),
    /// Answer the prompt with a JSON-RPC error.
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
#[derive(Default, Debug)]
pub struct Observed {
    pub prompts: Vec<String>,
    pub sessions: Vec<Value>,
    pub permission_outcomes: Vec<Value>,
    pub tool_results: Vec<Result<Value, String>>,
    pub cancels: usize,
}

#[derive(Clone)]
pub struct FakeAgent {
    pub script: Arc<Script>,
    pub gates: Arc<Gates>,
    pub observed: Arc<Mutex<Observed>>,
    /// Signalled by `Step::Exit` in-process.
    pub kill: Arc<Notify>,
    pub is_binary: bool,
}

impl FakeAgent {
    pub fn new(script: Script) -> Self {
        FakeAgent {
            script: Arc::new(script),
            gates: Arc::default(),
            observed: Arc::default(),
            kill: Arc::default(),
            is_binary: false,
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

    /// The agent side as a connectable component.
    pub fn component(self) -> impl ConnectTo<Client> {
        let mcp: Arc<Mutex<Option<(String, String)>>> = Arc::default();
        let session_id = Arc::new(Mutex::new(String::new()));
        let (me1, me2, me3, me4) = (self.clone(), self.clone(), self.clone(), self.clone());
        let (mcp2, sid2, sid3) = (mcp.clone(), session_id.clone(), session_id.clone());
        Agent
            .builder()
            .name("fake-acp-agent")
            .on_receive_request(
                async move |req: v1::InitializeRequest,
                            responder: Responder<v1::InitializeResponse>,
                            _cx| {
                    let _ = &me1;
                    let mut caps = v1::AgentCapabilities::new();
                    caps.mcp_capabilities = v1::McpCapabilities::new().http(me1.script.mcp_http);
                    let _ = req;
                    responder.respond(
                        v1::InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(caps)
                            .agent_info(v1::Implementation::new("fake-acp-agent", "0.0.0")),
                    )
                },
                acp::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v1::NewSessionRequest,
                            responder: Responder<v1::NewSessionResponse>,
                            _cx| {
                    me2.observed
                        .lock()
                        .expect("lock")
                        .sessions
                        .push(serde_json::to_value(&req).unwrap_or_default());
                    if let Some(msg) = &me2.script.fail_session_new {
                        return responder.respond_with_error(acp::Error::new(-32000, msg.clone()));
                    }
                    for server in &req.mcp_servers {
                        if let v1::McpServer::Http(h) = server {
                            let token = h
                                .headers
                                .iter()
                                .find(|x| x.name.eq_ignore_ascii_case("authorization"))
                                .map(|x| x.value.trim_start_matches("Bearer ").to_string())
                                .unwrap_or_default();
                            *mcp2.lock().expect("lock") = Some((h.url.clone(), token));
                        }
                    }
                    let id = format!(
                        "fake-session-{}",
                        me2.observed.lock().expect("lock").sessions.len()
                    );
                    *sid2.lock().expect("lock") = id.clone();
                    responder.respond(v1::NewSessionResponse::new(id))
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
                    let n = {
                        let mut o = me3.observed.lock().expect("lock");
                        o.prompts.push(text.clone());
                        o.prompts.len() - 1
                    };
                    let turn = me3.turn(n);
                    let me = me3.clone();
                    let mcp = mcp.clone();
                    let sid = sid3.lock().expect("lock").clone();
                    tokio::spawn(async move {
                        me.run_turn(turn, n, text, sid, mcp, responder, cx).await;
                    });
                    Ok(())
                },
                acp::on_receive_request!(),
            )
            .on_receive_notification(
                async move |_n: v1::CancelNotification, _cx| {
                    me4.observed.lock().expect("lock").cancels += 1;
                    Ok(())
                },
                acp::on_receive_notification!(),
            )
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        self,
        turn: Turn,
        n: usize,
        prompt: String,
        sid: String,
        mcp: Arc<Mutex<Option<(String, String)>>>,
        responder: Responder<v1::PromptResponse>,
        cx: acp::ConnectionTo<Client>,
    ) {
        let trigger = owed_trigger(&prompt);
        let session = v1::SessionId::new(sid);
        for step in turn.steps {
            match step {
                Step::Text(t) => {
                    let _ = cx.send_notification(v1::SessionNotification::new(
                        session.clone(),
                        v1::SessionUpdate::AgentMessageChunk(v1::ContentChunk::new(
                            v1::ContentBlock::Text(v1::TextContent::new(t)),
                        )),
                    ));
                }
                Step::ToolCall(title) => {
                    let _ = cx.send_notification(v1::SessionNotification::new(
                        session.clone(),
                        v1::SessionUpdate::ToolCall(v1::ToolCall::new(
                            v1::ToolCallId::new(format!("call-{n}")),
                            title,
                        )),
                    ));
                }
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
                Step::Permission(title) => {
                    let call = v1::ToolCallUpdate::new(
                        v1::ToolCallId::new(format!("perm-{n}")),
                        v1::ToolCallUpdateFields::new().title(title),
                    );
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
                    self.observed
                        .lock()
                        .expect("lock")
                        .permission_outcomes
                        .push(
                            outcome
                                .map(|o| serde_json::to_value(o).unwrap_or_default())
                                .unwrap_or_else(|e| Value::String(e.to_string())),
                        );
                }
                Step::Gate(name) => {
                    if !self.is_binary {
                        self.gates.wait(&name).await;
                    }
                }
                Step::SleepMs(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
                Step::Error(msg) => {
                    let _ = responder.respond_with_error(acp::Error::new(-32603, msg));
                    return;
                }
                Step::Exit => {
                    if self.is_binary {
                        std::process::exit(3);
                    }
                    // Wakes the running connection only; no permit is stored, so
                    // the next run of the agent starts clean.
                    self.kill.notify_waiters();
                    return;
                }
            }
        }
        let stop = match turn.stop.as_deref() {
            Some("cancelled") => v1::StopReason::Cancelled,
            Some("refusal") => v1::StopReason::Refusal,
            _ => v1::StopReason::EndTurn,
        };
        let _ = responder.respond(v1::PromptResponse::new(stop));
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
