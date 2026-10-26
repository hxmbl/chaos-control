//! ACP **v2 draft** session driver (`unstable_protocol_v2`).
//!
//! Status: draft protocol. claude-agent-acp 0.97.0 selects v2 when offered;
//! live-verified against it signed out: initialize, `session/new` with the HTTP
//! chat MCP server, prompt acceptance and `state_update` running -> idle (with
//! an `error` stop: "Authentication required"). Agent output, tool calls and
//! permission requests over v2 are not verified live yet (they need a signed-in
//! agent). codex-acp 0.16.0 answers v1.
//!
//! Acceptance point: the `session/prompt` response (v2 acknowledges acceptance
//! there). The turn ends at the next `state_update` of kind `idle`.

use std::sync::{Arc, Mutex};

use agent_client_protocol::{
    self as acp, Agent, Client, DynConnectTo, InitializeOutcome, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

use crate::acp::{ChatToolsPreference, Tracker, V2Open, choose_mcp, error_kind};
use crate::adapter::*;
use crate::types::BatchId;

enum Cmd {
    Prompt {
        turn: BatchId,
        text: Arc<str>,
        done: oneshot::Sender<Result<(), AgentError>>,
    },
    Cancel,
    Close,
}

struct V2Session {
    info: SessionInfo,
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl AgentSession for V2Session {
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

enum Ready {
    Session(SessionInfo),
    Fallback,
    Failed(AgentError),
}

fn mcp_servers(kind: ChatToolsTransport, ep: &ChatToolsEndpoint) -> Vec<v2::McpServer> {
    match kind {
        ChatToolsTransport::Http => vec![v2::McpServer::Http(
            v2::McpServerHttp::new("chaos-control", ep.url.clone()).headers(vec![
                v2::HttpHeader::new("Authorization", format!("Bearer {}", ep.token)),
            ]),
        )],
        ChatToolsTransport::StdioBridge => {
            let b = ep.bridge.as_ref().expect("bridge chosen only when present");
            vec![v2::McpServer::Stdio(
                v2::McpServerStdio::new("chaos-control", b.command.clone())
                    .args(b.args.clone())
                    .env(vec![
                        v2::EnvVariable::new("CHAOS_MCP_URL", ep.url.clone()),
                        v2::EnvVariable::new("CHAOS_MCP_TOKEN", ep.token.clone()),
                    ]),
            )]
        }
        ChatToolsTransport::None => Vec::new(),
    }
}

fn decline(req: &v2::RequestPermissionRequest) -> (v2::RequestPermissionResponse, Option<Arc<str>>) {
    let pick = req
        .options
        .iter()
        .find(|o| o.kind == v2::PermissionOptionKind::RejectOnce)
        .or_else(|| {
            req.options
                .iter()
                .find(|o| o.kind == v2::PermissionOptionKind::RejectAlways)
        });
    match pick {
        Some(o) => (
            v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Selected(
                v2::SelectedPermissionOutcome::new(o.option_id.clone()),
            )),
            Some(Arc::from(o.name.as_str())),
        ),
        None => (
            v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled),
            None,
        ),
    }
}

fn outcome(stop: Option<v2::StopReason>) -> TurnOutcome {
    match stop {
        None | Some(v2::StopReason::EndTurn) => TurnOutcome::Completed,
        Some(v2::StopReason::Cancelled) => TurnOutcome::Cancelled,
        Some(v2::StopReason::Refusal) => TurnOutcome::Refused,
        Some(other) => TurnOutcome::Other(Arc::from(format!("{other:?}"))),
    }
}

pub(crate) async fn open(
    transport: DynConnectTo<Client>,
    req: &OpenSession,
    pref: ChatToolsPreference,
) -> Result<Arc<dyn AgentSession>, V2Open> {
    let (ready_tx, ready_rx) = oneshot::channel::<Ready>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let tracker = Arc::new(Mutex::new(Tracker::default()));
    let events = req.events.clone();
    let cwd = req.cwd.clone();
    let endpoint = req.chat_tools.clone();
    tokio::spawn(run(transport, cwd, endpoint, pref, tracker, cmd_rx, ready_tx, events));
    match ready_rx.await {
        Ok(Ready::Session(info)) => Ok(Arc::new(V2Session { info, cmd: cmd_tx })),
        Ok(Ready::Fallback) => Err(V2Open::Fallback),
        Ok(Ready::Failed(e)) => Err(V2Open::Failed(e)),
        Err(_) => Err(V2Open::Failed(AgentError::new(
            AgentErrorKind::Spawn,
            "agent exited during setup",
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    transport: DynConnectTo<Client>,
    cwd: std::path::PathBuf,
    endpoint: ChatToolsEndpoint,
    pref: ChatToolsPreference,
    tracker: Arc<Mutex<Tracker>>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    ready_tx: oneshot::Sender<Ready>,
    events: EventSink,
) {
    let ready = Arc::new(Mutex::new(Some(ready_tx)));
    let ready_main = ready.clone();
    let (t_notif, t_perm, t_main) = (tracker.clone(), tracker.clone(), tracker.clone());
    let (ev_notif, ev_perm, ev_main) = (events.clone(), events.clone(), events.clone());
    // Set once the session is handed out; only then do exits become events.
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started_main = started.clone();
    let send_ready = move |r: Ready| {
        if let Some(tx) = ready_main.lock().expect("lock").take() {
            if matches!(r, Ready::Session(_)) {
                started_main.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let _ = tx.send(r);
        }
    };

    let result = Client
        .v2()
        .name("chaos-control")
        .on_receive_notification(
            async move |n: v2::UpdateSessionNotification, _cx: V2ConnectionTo<Agent>| {
                let mut out = Vec::new();
                {
                    let mut t = t_notif.lock().expect("lock");
                    match &n.update {
                        v2::SessionUpdate::AgentMessageChunk(chunk) if t.turn.is_some() => {
                            if let v2::ContentBlock::Text(text) = &chunk.content {
                                t.text.push_str(&text.text);
                            }
                        }
                        v2::SessionUpdate::ToolCallUpdate(_) if t.turn.is_some() => {
                            t.text.clear();
                        }
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                            if t.accepted =>
                        {
                            if let Some(turn) = t.turn.take() {
                                // An `error` stop is a failed turn (e.g. claude-agent-acp
                                // signed out: -32000 "Authentication required").
                                if let Some(v2::StopReason::Error(e)) = &idle.stop_reason {
                                    let error = match e.error.as_deref() {
                                        Some(err) => {
                                            let kind = if crate::acp::auth_like(&err.message)
                                                || i32::from(err.code) == -32000
                                            {
                                                AgentErrorKind::Auth
                                            } else {
                                                AgentErrorKind::Protocol
                                            };
                                            AgentError::new(kind, err.message.clone())
                                        }
                                        None => AgentError::new(AgentErrorKind::Protocol, "turn failed"),
                                    };
                                    t.text.clear();
                                    out.push(AgentEvent::TurnFailed { turn, accepted: true, error });
                                } else {
                                    out.push(AgentEvent::TurnEnded {
                                        turn,
                                        outcome: outcome(idle.stop_reason.clone()),
                                        final_text: Arc::from(std::mem::take(&mut t.text)),
                                    });
                                }
                            }
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
            async move |r: v2::RequestPermissionRequest, responder, _cx: V2ConnectionTo<Agent>| {
                let (response, declined_with) = decline(&r);
                let turn = t_perm.lock().expect("lock").turn;
                ev_perm.emit(AgentEvent::PermissionDeclined {
                    turn,
                    title: Arc::from(r.title.as_str()),
                    declined_with,
                });
                responder.respond(response)
            },
            acp::on_receive_request!(),
        )
        .connect_with(transport, async move |conn: V2ConnectionTo<Agent>| {
            let init = conn
                .send_initialize(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("chaos-control", env!("CARGO_PKG_VERSION")),
                ))
                .block_task()
                .await;
            let init = match init {
                Ok(InitializeOutcome::Initialized(i)) => i,
                // A v1 answer, or a refusal of v2: try v1 on a fresh connection.
                Ok(_) | Err(_) => {
                    send_ready(Ready::Fallback);
                    return Ok(());
                }
            };
            let http_ok = init
                .capabilities
                .session
                .as_ref()
                .and_then(|s| s.mcp.as_ref())
                .is_some_and(|m| m.http.is_some());
            let chat_tools = choose_mcp(pref, http_ok, &endpoint);
            let mut caps = Caps::CHAT_TOOLS_STDIO | Caps::INTERRUPT | Caps::BACKGROUND_WORK;
            if http_ok {
                caps.insert(Caps::CHAT_TOOLS_HTTP);
            }
            let new = conn
                .send_request(
                    v2::NewSessionRequest::new(cwd).mcp_servers(mcp_servers(chat_tools, &endpoint)),
                )
                .block_task()
                .await;
            let new = match new {
                Ok(n) => n,
                Err(e) => {
                    send_ready(Ready::Failed(AgentError::new(
                        error_kind(&e),
                        format!("session/new: {e}"),
                    )));
                    return Ok(());
                }
            };
            let session_id = new.session_id.clone();
            send_ready(Ready::Session(SessionInfo {
                caps,
                protocol: Arc::from("acp/v2-draft"),
                native_ref: Arc::from(session_id.to_string()),
                agent_name: Some(Arc::from(format!("{} {}", init.info.name, init.info.version))),
                unused_extensions: Vec::new(),
                chat_tools,
            }));
            loop {
                // The agent closing its end (process exit) ends the session.
                let cmd = tokio::select! {
                    cmd = cmd_rx.recv() => cmd,
                    _ = conn.incoming_closed() => None,
                };
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::Prompt { turn, text, done } => {
                        *t_main.lock().expect("lock") = Tracker {
                            turn: Some(turn),
                            accepted: false,
                            text: String::new(),
                        };
                        let sent = conn.send_request(v2::PromptRequest::new(
                            session_id.clone(),
                            vec![v2::ContentBlock::Text(v2::TextContent::new(text.to_string()))],
                        ));
                        let _ = done.send(Ok(()));
                        let t = t_main.clone();
                        let ev = ev_main.clone();
                        tokio::spawn(async move {
                            let result = sent.block_task().await;
                            let event = {
                                let mut tr = t.lock().expect("lock");
                                if tr.turn != Some(turn) {
                                    return;
                                }
                                match result {
                                    Ok(_) => tr.accept(),
                                    Err(e) => {
                                        tr.turn = None;
                                        Some(AgentEvent::TurnFailed {
                                            turn,
                                            accepted: false,
                                            error: AgentError::new(error_kind(&e), format!("{e}")),
                                        })
                                    }
                                }
                            };
                            if let Some(e) = event {
                                ev.emit(e);
                            }
                        });
                    }
                    Cmd::Cancel => {
                        let _ = conn.send_notification(v2::CancelSessionNotification::new(session_id.clone()));
                    }
                    Cmd::Close => break,
                }
            }
            Ok(())
        })
        .await;

    if let Some(tx) = ready.lock().expect("lock").take() {
        let _ = tx.send(Ready::Failed(AgentError::new(
            AgentErrorKind::Spawn,
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "agent exited during setup".into()),
        )));
        return;
    }
    if !started.load(std::sync::atomic::Ordering::SeqCst) {
        return; // fallback or setup failure, reported through `open`
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
