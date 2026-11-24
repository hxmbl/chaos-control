//! Native OpenCode V2 adapter (STRM-612).
//!
//! Connects to an OpenCode v2 server (`opencode serve`) over HTTP/SSE instead
//! of speaking ACP (`opencode acp`). This is intentionally separate from the
//! ACP adapters so existing ACP paths remain intact.
//!
//! Status: initial slice for the STRM-612 ship-today window. Capabilities and
//! enforcement guarantees must be reported honestly; unsupported or unverified
//! behavior must not be presented as protected.

use std::collections::HashMap;
use std::{path::PathBuf, sync::Arc};

use futures::{StreamExt, future::BoxFuture};
use tokio::io::AsyncBufReadExt;

use crate::{
    adapter::{
        AgentAdapter, AgentError, AgentErrorKind, AgentEvent, AgentSession, Caps,
        ChatToolsTransport, EventSink, OpenSession, Probe, SessionInfo, TurnOutcome,
    },
    approval::{
        ApprovalBroker, ApprovalChoices, ApprovalDecisionKind, ApprovalId, ApprovalOperation,
        ApprovalOutcome, ApprovalOutcomeState, ApprovalProvider, ApprovalRequest,
        ApprovalResolution, ApprovalScope,
    },
    harness::ModelInfo,
    types::{BatchId, ChannelId, MemberId},
};

/// How to reach an OpenCode v2 server.
#[derive(Clone, Debug)]
pub struct OpenCodeV2Config {
    /// `opencode` binary, used to start a server when `server_url` is absent.
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Connect to an existing OpenCode v2 server instead of starting one.
    pub server_url: Option<String>,
    /// Password for HTTP basic auth (user `opencode`), if required.
    pub server_password: Option<String>,
    /// Harness-native model id to select for the session (e.g. `provider/model`
    /// or `model`; defaults provider to `opencode` when omitted). Applied after
    /// session create when set.
    pub model: Option<Arc<str>>,
}

pub struct OpenCodeV2Adapter {
    config: OpenCodeV2Config,
}

impl OpenCodeV2Adapter {
    pub fn new(config: OpenCodeV2Config) -> Self {
        OpenCodeV2Adapter { config }
    }
}

impl AgentAdapter for OpenCodeV2Adapter {
    fn describe(&self) -> String {
        match &self.config.server_url {
            Some(url) => format!("opencode-v2: {url}"),
            None => format!(
                "opencode-v2: {} {}",
                self.config.command.display(),
                self.config.args.join(" ")
            ),
        }
    }

    fn probe(&self) -> BoxFuture<'_, Probe> {
        let config = self.config.clone();
        Box::pin(async move {
            if config.server_url.is_some() {
                // Reachability is verified on open; probing a URL here would add
                // network side effects. Report plausible so configured endpoints
                // are selectable.
                return Probe::Plausible;
            }
            if crate::acp::resolve_command(&config.command).is_some() {
                Probe::Plausible
            } else {
                Probe::Missing {
                    why: Arc::from(format!("`{}` not found on PATH", config.command.display())),
                }
            }
        })
    }

    fn open(&self, req: OpenSession) -> BoxFuture<'_, Result<Arc<dyn AgentSession>, AgentError>> {
        let config = self.config.clone();
        Box::pin(async move { open_session(config, req).await })
    }

    fn list_models(
        &self,
        cwd: PathBuf,
    ) -> BoxFuture<'_, Option<Result<Vec<ModelInfo>, AgentError>>> {
        let config = self.config.clone();
        Box::pin(async move { Some(list_models(config, cwd).await) })
    }
}

/// Shared HTTP surface for one session. Basic auth is attached per request:
/// the server password may be absent, and the transport should work either way.
struct Http {
    client: reqwest::Client,
    /// Base URL without a trailing slash.
    base: String,
    password: Option<String>,
}

impl Http {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.client.request(method, format!("{}{path}", self.base));
        if let Some(password) = &self.password {
            req = req.basic_auth("opencode", Some(password));
        }
        req
    }

    async fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AgentError> {
        self.send_json(reqwest::Method::POST, path, body).await
    }

    async fn put_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AgentError> {
        self.send_json(reqwest::Method::PUT, path, body).await
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, AgentError> {
        let resp = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(transport_error)?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(AgentError::new(
                AgentErrorKind::Protocol,
                format!("GET {path} failed ({status}): {detail}"),
            ));
        }
        let body = resp.text().await.unwrap_or_default();
        if body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&body).map_err(|e| {
            AgentError::new(AgentErrorKind::Protocol, format!("GET {path}: {e}: {body}"))
        })
    }

    async fn send_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AgentError> {
        let resp = self
            .request(method.clone(), path)
            .json(body)
            .send()
            .await
            .map_err(transport_error)?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(AgentError::new(
                AgentErrorKind::Protocol,
                format!("{method} {path} failed ({status}): {detail}"),
            ));
        }
        let body = resp.text().await.unwrap_or_default();
        if body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&body).map_err(|e| {
            AgentError::new(
                AgentErrorKind::Protocol,
                format!("{method} {path}: {e}: {body}"),
            )
        })
    }
}

fn transport_error(e: reqwest::Error) -> AgentError {
    AgentError::new(AgentErrorKind::Transport, e.to_string())
}

fn model_id_from_parts(provider_id: &str, model_id: &str) -> String {
    if provider_id == "opencode" {
        format!("opencode/{model_id}")
    } else {
        format!("opencode/{provider_id}/{model_id}")
    }
}

fn model_info_from_value(value: &serde_json::Value) -> Option<ModelInfo> {
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let provider_id = value
        .get("providerID")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let model_id = value
        .get("modelID")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let id = match (provider_id, model_id) {
        (Some(provider), Some(model)) => model_id_from_parts(provider, model),
        _ => {
            let raw = value
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())?;
            if raw.starts_with("opencode/") {
                raw.to_owned()
            } else {
                format!("opencode/{raw}")
            }
        }
    };
    Some(ModelInfo::new(Some(&id), name))
}

fn parse_model_list(value: serde_json::Value) -> Vec<ModelInfo> {
    let items = value
        .get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .or_else(|| value.as_array().cloned())
        .unwrap_or_default();
    let mut models = Vec::new();
    for item in &items {
        if let Some(info) = model_info_from_value(item) {
            models.push(info);
        }
    }
    disambiguate_model_names(&mut models);
    models
}

fn disambiguate_model_names(models: &mut [ModelInfo]) {
    use std::collections::HashMap;
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    for m in models.iter() {
        if let Some(id) = m.id.as_deref() {
            let provider = id
                .strip_prefix("opencode/")
                .and_then(|rest| rest.split_once('/').map(|(p, _)| p))
                .unwrap_or("opencode")
                .to_string();
            *counts
                .entry((provider, m.name.as_ref().to_string()))
                .or_insert(0) += 1;
        }
    }
    for m in models.iter_mut() {
        let Some(id) = m.id.as_deref() else { continue };
        let provider = id
            .strip_prefix("opencode/")
            .and_then(|rest| rest.split_once('/').map(|(p, _)| p))
            .unwrap_or("opencode")
            .to_string();
        let key = (provider, m.name.as_ref().to_string());
        if counts.get(&key).copied().unwrap_or(0) > 1 {
            let n = seen.entry(key).or_insert(0);
            *n += 1;
            if *n > 1 {
                m.name = Arc::from(format!("{} :{n}", m.name.as_ref()));
            }
        }
    }
}

async fn fetch_models(http: &Http) -> Result<Vec<ModelInfo>, AgentError> {
    let value = http.get_json("/api/model").await?;
    Ok(parse_model_list(value))
}

async fn list_models(config: OpenCodeV2Config, cwd: PathBuf) -> Result<Vec<ModelInfo>, AgentError> {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let (base, generated, mut child) = start_server(&config, &cwd).await?;
        let password = config
            .server_password
            .clone()
            .or_else(|| std::env::var("OPENCODE_SERVER_PASSWORD").ok())
            .or(generated);
        let http = Http {
            client: reqwest::Client::new(),
            base,
            password,
        };
        let result = fetch_models(&http).await;
        if let Some(child) = child.as_mut() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        result
    })
    .await
    .unwrap_or_else(|_| {
        Err(AgentError::new(
            AgentErrorKind::Transport,
            "no answer while listing models",
        ))
    })
}

struct SessionInner {
    http: Http,
    session_id: Arc<str>,
    agent: MemberId,
    channel: ChannelId,
    approval_broker: Option<Arc<dyn ApprovalBroker>>,
    mapper: std::sync::Mutex<EventMapper>,
    events: EventSink,
    /// The server we started ourselves, when `server_url` was not configured.
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    sse: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct OpenCodeV2Session {
    info: SessionInfo,
    inner: Arc<SessionInner>,
}

impl OpenCodeV2Session {
    fn emit(&self, event: AgentEvent) {
        self.inner.events.emit(event);
    }
}

impl AgentSession for OpenCodeV2Session {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    fn deliver(&self, turn: BatchId, prompt: Arc<str>) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            // The mapper must know the turn before any event for it arrives.
            self.inner.mapper.lock().unwrap().begin(turn);
            let body = serde_json::json!({ "text": prompt });
            let path = format!("/api/session/{}/prompt", self.inner.session_id);
            match self.inner.http.post_json(&path, &body).await {
                Ok(_) => {
                    // Admission is durable, so `Accepted` is only claimed once
                    // the server has taken the prompt.
                    self.emit(AgentEvent::Accepted { turn });
                    Ok(())
                }
                Err(error) => {
                    self.inner.mapper.lock().unwrap().take_turn();
                    self.emit(AgentEvent::TurnFailed {
                        turn,
                        accepted: false,
                        error: error.clone(),
                    });
                    Err(error)
                }
            }
        })
    }

    fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            let path = format!("/api/session/{}/interrupt", self.inner.session_id);
            let result = self
                .inner
                .http
                .post_json(&path, &serde_json::json!({}))
                .await
                .map(|_| ());
            if result.is_ok() {
                // The interrupted turn can no longer resolve later decisions;
                // fail this session's pending approvals closed.
                cancel_session_approvals(&self.inner, ApprovalResolution::Disconnect).await;
            }
            result
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Some(sse) = self.inner.sse.lock().await.take() {
                sse.abort();
            }
            if let Some(mut child) = self.inner.child.lock().await.take() {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            cancel_session_approvals(&self.inner, ApprovalResolution::Disconnect).await;
        })
    }
}

/// Fail closed only this session's pending approvals. Other agents' requests
/// stay pending and answerable when one session disconnects or is interrupted.
async fn cancel_session_approvals(inner: &SessionInner, resolution: ApprovalResolution) {
    if let Some(broker) = &inner.approval_broker {
        let _ = broker
            .cancel_pending_for(
                ApprovalScope::new(ApprovalProvider::OpenCodeV2, inner.session_id.clone()),
                resolution,
            )
            .await;
    }
}

/// Reads `/api/event` and turns frames into [`AgentEvent`]s until the stream
/// ends, then reports the turn that was in flight (if any) as failed.
async fn stream_events(inner: Arc<SessionInner>) {
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        inner
            .http
            .request(reqwest::Method::GET, "/api/event")
            .send()
            .await
    })
    .await
    .map_err(|_| AgentError::new(AgentErrorKind::Transport, "event stream did not open"))
    .and_then(|resp| resp.map_err(transport_error))
    .and_then(|resp| {
        let status = resp.status();
        if status.is_success() {
            Ok(resp)
        } else {
            Err(AgentError::new(
                AgentErrorKind::Protocol,
                format!("event stream rejected ({status})"),
            ))
        }
    });
    let resp = match result {
        Ok(resp) => resp,
        Err(error) => {
            cancel_session_approvals(&inner, ApprovalResolution::Disconnect).await;
            fail_in_flight(&inner, &error);
            return;
        }
    };

    let mut parser = SseParser::default();
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(e) => {
                cancel_session_approvals(&inner, ApprovalResolution::Disconnect).await;
                fail_in_flight(&inner, &transport_error(e));
                return;
            }
        };
        parser.push(&bytes);
        while let Some(frame) = parser.next_frame() {
            inner.mapper.lock().unwrap().observe(&frame);
            // Awaiting a user decision inline would stall this SSE loop, so
            // approvals are handled on their own task while events keep flowing.
            tokio::spawn(answer_permission(Arc::clone(&inner), frame.data.clone()));
            let events = inner.mapper.lock().unwrap().handle(&frame);
            for event in events {
                inner.events.emit(event);
            }
        }
    }
    fail_in_flight(
        &inner,
        &AgentError::new(AgentErrorKind::Transport, "event stream closed"),
    );
    cancel_session_approvals(&inner, ApprovalResolution::Disconnect).await;
    inner.events.emit(AgentEvent::Exited {
        error: Some("opencode v2 event stream closed".into()),
    });
}

/// Sends no provider reply unless a CC approval broker resolves it explicitly.
fn approval_decision_is_correlated(
    expected: &ApprovalId,
    decision: &crate::approval::ApprovalDecision,
) -> bool {
    decision.cc_request_id == *expected
}

async fn answer_permission(inner: Arc<SessionInner>, frame_data: String) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&frame_data) else {
        return;
    };
    if value.get("type").and_then(|t| t.as_str()) != Some("permission.asked") {
        return;
    }
    let data = value.get("data").unwrap_or(&value);
    if data.get("sessionID").and_then(|s| s.as_str()) != Some(&*inner.session_id) {
        return;
    }
    let Some(request) = data.get("id").and_then(|i| i.as_str()) else {
        return;
    };
    let cc_request_id = ApprovalId::from(format!("opencode-v2:{}:{}", inner.session_id, request));
    let source = data.get("source").cloned();
    let source_id = source
        .as_ref()
        .and_then(|s| s.get("id"))
        .and_then(|id| id.as_str())
        .map(Arc::<str>::from);
    let action = data
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or("unknown");
    let arguments = source_id.as_ref().and_then(|id| {
        inner
            .mapper
            .lock()
            .unwrap()
            .tool_arguments
            .get(id.as_ref())
            .cloned()
    });
    let turn = inner.mapper.lock().unwrap().turn;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let Some(broker) = inner.approval_broker.as_ref() else {
        // Fail closed, but never silently: the operation was not approved and
        // no provider reply was sent because no CC approval authority exists.
        let reason = format!(
            "opencode-v2: approval broker unavailable — permission request {request} in session {} left unanswered (unverified, fail-closed)",
            inner.session_id
        );
        eprintln!("{reason}");
        inner.events.emit(AgentEvent::Activity {
            label: Arc::from(reason),
        });
        return;
    };
    let approval_request = ApprovalRequest {
        cc_request_id: cc_request_id.clone(),
        provider: ApprovalProvider::OpenCodeV2,
        provider_request_ref: Arc::from(request),
        session_id: inner.session_id.clone(),
        agent: inner.agent,
        channel: inner.channel,
        turn,
        operation: ApprovalOperation {
            action: Arc::from(action),
            tool_name: source
                .as_ref()
                .and_then(|s| s.get("type"))
                .and_then(|t| t.as_str())
                .filter(|t| *t == "tool")
                .map(|_| Arc::from(action)),
            summary: data
                .get("resources")
                .and_then(|r| r.as_array())
                .and_then(|r| r.first())
                .and_then(|r| r.as_str())
                .map(Arc::<str>::from),
            arguments,
            raw: Some(value.clone()),
        },
        choices: ApprovalChoices::DecisionVocabulary {
            decisions: vec![Arc::from("reject"), Arc::from("once"), Arc::from("always")],
            raw: Some(value.clone()),
        },
        requested_at: now,
        context: source,
    };
    let decision = match broker.request_decision(approval_request).await {
        Ok(decision) => decision,
        Err(error) => {
            // A replay of an already-resolved request fails closed: the
            // original outcome stands and is not overwritten as Failed.
            if !error.message.as_ref().contains("already resolved") {
                let _ = broker
                    .record_outcome(ApprovalOutcome {
                        cc_request_id,
                        state: ApprovalOutcomeState::Failed,
                        provider_reply: None,
                        execution_effect: None,
                        error: Some(Arc::from("approval broker failed")),
                        recorded_at: Some(now),
                    })
                    .await;
            }
            return;
        }
    };
    if !approval_decision_is_correlated(&cc_request_id, &decision) {
        eprintln!(
            "opencode-v2 approval correlation mismatch: expected {}, got {}; no provider reply sent",
            cc_request_id.as_str(),
            decision.cc_request_id.as_str()
        );
        let _ = broker
            .record_outcome(ApprovalOutcome {
                cc_request_id,
                state: ApprovalOutcomeState::Failed,
                provider_reply: None,
                execution_effect: None,
                error: Some(Arc::from("approval decision correlation mismatch")),
                recorded_at: Some(now),
            })
            .await;
        return;
    }
    if decision.cc_request_id != cc_request_id {
        // A decision for another request must never authorize this operation.
        eprintln!(
            "opencode-v2: approval decision id mismatch (expected {}, got {}) — failing closed without a provider reply",
            cc_request_id.as_str(),
            decision.cc_request_id.as_str()
        );
        return;
    }
    let choice = match decision.decision {
        ApprovalDecisionKind::Selected { choice } => choice,
        ApprovalDecisionKind::NotResolved { resolution } => {
            let resolution_name = match resolution {
                ApprovalResolution::Lost => "lost",
                ApprovalResolution::Timeout => "timeout",
                ApprovalResolution::Disconnect => "disconnect",
                ApprovalResolution::Cancelled => "cancelled",
            };
            let _ = broker
                .record_outcome(ApprovalOutcome {
                    cc_request_id: decision.cc_request_id,
                    state: ApprovalOutcomeState::NotSent,
                    provider_reply: None,
                    execution_effect: None,
                    error: Some(Arc::from(format!("approval unresolved: {resolution_name}"))),
                    recorded_at: Some(now),
                })
                .await;
            return;
        }
    };
    if !["reject", "once", "always"].contains(&choice.as_ref()) {
        let _ = broker
            .record_outcome(ApprovalOutcome {
                cc_request_id: decision.cc_request_id,
                state: ApprovalOutcomeState::Failed,
                provider_reply: None,
                execution_effect: None,
                error: Some(Arc::from("invalid OpenCode v2 decision")),
                recorded_at: Some(now),
            })
            .await;
        return;
    }
    let path = format!(
        "/api/session/{}/permission/{request}/reply",
        inner.session_id
    );
    match inner
        .http
        .post_json(&path, &serde_json::json!({ "decision": choice.as_ref() }))
        .await
    {
        Ok(provider_reply) => {
            let state = if choice.as_ref() == "reject" {
                ApprovalOutcomeState::Denied
            } else {
                ApprovalOutcomeState::Allowed
            };
            let _ = broker
                .record_outcome(ApprovalOutcome {
                    cc_request_id: decision.cc_request_id,
                    state,
                    provider_reply: Some(provider_reply),
                    execution_effect: None,
                    error: None,
                    recorded_at: Some(now),
                })
                .await;
        }
        Err(error) => {
            let _ = broker
                .record_outcome(ApprovalOutcome {
                    cc_request_id: decision.cc_request_id,
                    state: ApprovalOutcomeState::Failed,
                    provider_reply: None,
                    execution_effect: None,
                    error: Some(error.message),
                    recorded_at: Some(now),
                })
                .await;
        }
    }
}

fn fail_in_flight(inner: &SessionInner, error: &AgentError) {
    if let Some(turn) = inner.mapper.lock().unwrap().take_turn() {
        inner.events.emit(AgentEvent::TurnFailed {
            turn,
            accepted: true,
            error: error.clone(),
        });
    }
}

/// Resolves the server URL, starting `opencode serve` when none was configured.
/// When we start it, the URL and generated password are read from its stdout,
/// since both are chosen at runtime.
async fn start_server(
    config: &OpenCodeV2Config,
    cwd: &std::path::Path,
) -> Result<(String, Option<String>, Option<tokio::process::Child>), AgentError> {
    if let Some(url) = &config.server_url {
        return Ok((url.trim_end_matches('/').to_owned(), None, None));
    }
    let mut command = tokio::process::Command::new(&config.command);
    if config.args.is_empty() {
        command.arg("serve");
    } else {
        command.args(&config.args);
    }
    command
        .envs(config.env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().map_err(|e| {
        AgentError::new(
            AgentErrorKind::Transport,
            format!("could not start `{}`: {e}", config.command.display()),
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::new(AgentErrorKind::Transport, "server stdout was not piped"))?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let found = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let (mut url, mut password) = (None, None);
        while let Ok(Some(line)) = lines.next_line().await {
            if url.is_none()
                && let Some(found) = line
                    .split_whitespace()
                    .find(|word| word.starts_with("http://") || word.starts_with("https://"))
            {
                url = Some(found.trim_end_matches('/').to_owned());
            }
            if let Some(rest) = line.strip_prefix("server password ") {
                password = Some(rest.trim().to_owned());
            }
            if url.is_some() && password.is_some() {
                break;
            }
        }
        url.map(|url| (url, password))
    })
    .await;
    match found {
        Ok(Some((url, password))) => Ok((url, password, Some(child))),
        _ => {
            let _ = child.start_kill();
            Err(AgentError::new(
                AgentErrorKind::Transport,
                "`opencode serve` did not report a URL on stdout",
            ))
        }
    }
}

/// One dispatched server-sent event, after the framing has been removed.
#[derive(Debug, Default, PartialEq, Eq)]
struct SseFrame {
    /// The `event:` field, when the server names its events.
    event: Option<String>,
    /// `data:` lines joined with `\n`, per the SSE spec.
    data: String,
}

/// Incremental SSE parser: the transport can hand over bytes as they arrive,
/// including partial frames and splits inside a multi-byte character.
#[derive(Default)]
struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Returns the next frame that carries data. Frames with only comments or
    /// unknown fields are skipped, as the spec requires.
    fn next_frame(&mut self) -> Option<SseFrame> {
        loop {
            let (end, sep) = frame_boundary(&self.buf)?;
            let block: Vec<u8> = self.buf.drain(..end + sep).take(end).collect();
            let frame = parse_block(&String::from_utf8_lossy(&block));
            if !frame.data.is_empty() {
                return Some(frame);
            }
        }
    }
}

/// Byte offset of the blank line that ends the next event, and its length.
fn frame_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

fn parse_block(block: &str) -> SseFrame {
    let mut frame = SseFrame::default();
    let mut data = Vec::new();
    for line in block.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => frame.event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    frame.data = data.join("\n");
    frame
}

/// Turns v2 server events for the in-flight turn into [`AgentEvent`]s.
///
/// Events are a `type` + `data` envelope on a server-wide stream, shaped by the
/// v2 OpenAPI document (`/openapi.json`, v2.3.2). Only what the adapter needs is
/// read: streamed assistant text and execution boundaries. Unknown event types
/// are ignored, so a server that grows new events cannot break a turn. Events
/// are accepted only when `data.sessionID` matches our session; a missing or
/// foreign id is dropped, since a shared server streams every session's events.
#[derive(Default)]
struct EventMapper {
    turn: Option<BatchId>,
    /// Our native session id, once known; `None` accepts every session.
    session: Option<String>,
    text: String,
    tool_arguments: HashMap<Arc<str>, serde_json::Value>,
}

impl EventMapper {
    fn begin(&mut self, turn: BatchId) {
        self.turn = Some(turn);
        self.text.clear();
    }

    fn observe(&mut self, frame: &SseFrame) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&frame.data) else {
            return;
        };
        let kind = value
            .get("type")
            .and_then(|k| k.as_str())
            .or(frame.event.as_deref())
            .unwrap_or_default();
        if kind != "session.tool.input.ended" && kind != "session.tool.called" {
            return;
        }
        let data = event_data(&value);
        if !self.is_ours(data) {
            return;
        }
        let id = data
            .get("id")
            .or_else(|| data.get("toolCallId"))
            .or_else(|| data.get("tool_call_id"))
            .and_then(|id| id.as_str());
        let arguments = data
            .get("input")
            .or_else(|| data.get("arguments"))
            .or_else(|| data.get("tool_input"))
            .cloned();
        if let (Some(id), Some(arguments)) = (id, arguments) {
            self.tool_arguments.insert(Arc::from(id), arguments);
        }
    }

    /// Removes and returns the turn that was in flight, if any.
    fn take_turn(&mut self) -> Option<BatchId> {
        self.text.clear();
        self.turn.take()
    }

    fn handle(&mut self, frame: &SseFrame) -> Vec<AgentEvent> {
        let value: serde_json::Value = match serde_json::from_str(&frame.data) {
            Ok(value) => value,
            Err(_) => return Vec::new(),
        };
        let kind = value
            .get("type")
            .and_then(|k| k.as_str())
            .or(frame.event.as_deref())
            .unwrap_or_default();
        let data = event_data(&value);
        if !self.is_ours(data) || self.turn.is_none() {
            return Vec::new();
        }
        let mut out = Vec::new();
        match kind {
            // Deltas are the live text; `text.ended` carries the final version.
            "session.text.delta" => {
                if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                    self.text.push_str(delta);
                }
            }
            "session.text.ended" => {
                if let Some(text) = data.get("text").and_then(|t| t.as_str()) {
                    self.text = text.to_owned();
                }
            }
            "session.execution.succeeded" => {
                if let Some(turn) = self.turn.take() {
                    let final_text: Arc<str> = Arc::from(self.text.as_str());
                    self.text.clear();
                    out.push(AgentEvent::TurnEnded {
                        turn,
                        outcome: TurnOutcome::Completed,
                        final_text,
                    });
                }
            }
            "session.execution.interrupted" => {
                if let Some(turn) = self.turn.take() {
                    let final_text: Arc<str> = Arc::from(self.text.as_str());
                    self.text.clear();
                    out.push(AgentEvent::TurnEnded {
                        turn,
                        outcome: TurnOutcome::Cancelled,
                        final_text,
                    });
                }
            }
            "session.execution.failed" => {
                if let Some(turn) = self.turn.take() {
                    let detail = data
                        .get("error")
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "session error".to_owned());
                    out.push(AgentEvent::TurnFailed {
                        turn,
                        accepted: true,
                        error: AgentError::new(AgentErrorKind::Protocol, detail),
                    });
                }
            }
            _ => {}
        }
        out
    }

    fn is_ours(&self, data: &serde_json::Value) -> bool {
        matches!(
            (&self.session, session_id(data)),
            (Some(mine), Some(theirs)) if mine == theirs
        )
    }
}

fn event_data(value: &serde_json::Value) -> &serde_json::Value {
    value
        .get("data")
        .or_else(|| value.get("properties"))
        .unwrap_or(value)
}

fn session_id(data: &serde_json::Value) -> Option<&str> {
    data.get("sessionID").and_then(|s| s.as_str())
}

/// A per-session name for the chat tools MCP entry. The experimental endpoint
/// replaces an existing registration under the same name, so sessions sharing
/// one server need names that do not collide.
fn chat_tools_mcp_entry_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("chaos-control-{:x}-{:x}-{}", std::process::id(), now, seq)
}

async fn open_session(
    config: OpenCodeV2Config,
    req: OpenSession,
) -> Result<Arc<dyn AgentSession>, AgentError> {
    let (base, generated, child) = start_server(&config, &req.cwd).await?;
    let password = config
        .server_password
        .clone()
        .or_else(|| std::env::var("OPENCODE_SERVER_PASSWORD").ok())
        .or(generated);
    let http = Http {
        client: reqwest::Client::new(),
        base,
        password,
    };

    // Chat tools are a server-level MCP entry, so they are installed before the
    // session exists and are available to its first turn.
    let chat_tools = req.chat_tools.url.trim();
    if !chat_tools.is_empty() {
        let mut headers = serde_json::Map::new();
        if !req.chat_tools.token.is_empty() {
            headers.insert(
                "Authorization".into(),
                format!("Bearer {}", req.chat_tools.token).into(),
            );
        }
        http.put_json(
            &format!("/api/experimental/mcp/{}", chat_tools_mcp_entry_name()),
            &serde_json::json!({
                "config": {
                    "type": "remote",
                    "url": chat_tools,
                    "headers": headers,
                },
            }),
        )
        .await?;
    }

    let created = http
        .post_json(
            "/api/session",
            &serde_json::json!({ "title": format!("chaos-control: {}", req.handle) }),
        )
        .await?;
    let id = created
        .get("data")
        .and_then(|d| d.get("id"))
        .or_else(|| created.get("id"))
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Protocol,
                "session.create returned no session id",
            )
        })?
        .to_owned();

    let mut applied_model = None;
    if let Some(model) = config.model.as_deref() {
        let native = model.strip_prefix("opencode/").unwrap_or(model);
        let (provider_id, model_id) = match native.split_once('/') {
            Some((provider, id)) if !provider.is_empty() && !id.is_empty() => (provider, id),
            _ => ("opencode", native),
        };
        http.post_json(
            &format!("/api/session/{id}/model"),
            &serde_json::json!({
                "model": {
                    "id": model_id,
                    "providerID": provider_id,
                }
            }),
        )
        .await?;
        applied_model = Some(Arc::from(model));
    }

    let mapper = EventMapper {
        session: Some(id.clone()),
        ..EventMapper::default()
    };
    let inner = Arc::new(SessionInner {
        http,
        session_id: Arc::from(id.as_str()),
        agent: req.agent,
        channel: req.channel,
        approval_broker: req.approval_broker,
        mapper: std::sync::Mutex::new(mapper),
        events: req.events.clone(),
        child: tokio::sync::Mutex::new(child),
        sse: tokio::sync::Mutex::new(None),
    });
    let models = fetch_models(&inner.http).await.unwrap_or_default();

    let sse = tokio::spawn(stream_events(inner.clone()));
    *inner.sse.lock().await = Some(sse);

    let info = SessionInfo {
        // Interrupt is verified by the server API; chat tools count only once
        // they were actually installed above.
        caps: if chat_tools.is_empty() {
            Caps::empty().union(Caps::INTERRUPT)
        } else {
            Caps::empty()
                .union(Caps::INTERRUPT)
                .union(Caps::CHAT_TOOLS_HTTP)
        },
        protocol: Arc::from("opencode/v2-http"),
        native_ref: Arc::from(id.as_str()),
        agent_name: Some(Arc::from("opencode")),
        unused_extensions: Vec::new(),
        chat_tools: if chat_tools.is_empty() {
            ChatToolsTransport::None
        } else {
            ChatToolsTransport::Http
        },
        model: applied_model,
        models,
        model_note: None,
    };
    Ok(Arc::new(OpenCodeV2Session { info, inner }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper_for(session: &str) -> EventMapper {
        EventMapper {
            session: Some(session.into()),
            ..EventMapper::default()
        }
    }

    #[test]
    fn tool_call_arguments_are_correlated_by_source_id() {
        let mut mapper = mapper_for("s1");
        mapper.observe(&SseFrame {
            event: None,
            data: r#"{"type":"session.tool.input.ended","data":{"sessionID":"s1","id":"tool-call-1","input":{"command":"echo OK"}}}"#
                .into(),
        });
        assert_eq!(
            mapper.tool_arguments.get("tool-call-1"),
            Some(&serde_json::json!({"command": "echo OK"}))
        );
    }

    #[test]
    fn approval_decision_must_match_the_request_id() {
        let expected = ApprovalId::from("opencode-v2:s1:req-1");
        let matching = crate::approval::ApprovalDecision {
            cc_request_id: expected.clone(),
            decision: ApprovalDecisionKind::Selected {
                choice: Arc::from("reject"),
            },
            decided_at: 1,
            actor: None,
        };
        let mismatched = crate::approval::ApprovalDecision {
            cc_request_id: ApprovalId::from("opencode-v2:s1:req-2"),
            decision: matching.decision.clone(),
            decided_at: 1,
            actor: None,
        };
        assert!(approval_decision_is_correlated(&expected, &matching));
        assert!(!approval_decision_is_correlated(&expected, &mismatched));
    }

    fn parse_all(chunks: &[&[u8]]) -> Vec<SseFrame> {
        let mut parser = SseParser::default();
        let mut frames = Vec::new();
        for chunk in chunks {
            parser.push(chunk);
            while let Some(frame) = parser.next_frame() {
                frames.push(frame);
            }
        }
        frames
    }

    #[test]
    fn frames_survive_arbitrary_chunking() {
        let wire = b"event: message.part.updated\r\ndata: {\"a\":1}\r\n\r\n: keepalive\n\ndata: one\ndata: two\n\n";
        let frames = parse_all(&[&wire[..7], &wire[7..20], &wire[20..]]);
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: Some("message.part.updated".into()),
                    data: "{\"a\":1}".into(),
                },
                SseFrame {
                    event: None,
                    data: "one\ntwo".into(),
                },
            ]
        );
    }

    #[test]
    fn incomplete_frame_is_held_until_finished() {
        let mut parser = SseParser::default();
        parser.push(b"data: {\"type\":");
        assert_eq!(parser.next_frame(), None);
        parser.push(b"\"session.execution.succeeded\"}\n\n");
        assert_eq!(
            parser.next_frame().map(|f| f.data),
            Some("{\"type\":\"session.execution.succeeded\"}".into())
        );
    }

    #[test]
    fn text_deltas_end_with_the_turn() {
        let mut mapper = mapper_for("s1");
        mapper.begin(BatchId(9));
        let frame = |data: String| SseFrame { event: None, data };
        let delta = |text: &str| {
            frame(format!(
                r#"{{"type":"session.text.delta","data":{{"sessionID":"s1","delta":"{text}"}}}}"#
            ))
        };
        assert!(mapper.handle(&delta("hello ")).is_empty());
        assert!(mapper.handle(&delta("there")).is_empty());
        let ended = frame(
            r#"{"type":"session.text.ended","data":{"sessionID":"s1","text":"hello there!"}}"#
                .into(),
        );
        assert!(mapper.handle(&ended).is_empty());
        let done =
            frame(r#"{"type":"session.execution.succeeded","data":{"sessionID":"s1"}}"#.into());
        match mapper.handle(&done).as_slice() {
            [
                AgentEvent::TurnEnded {
                    turn,
                    outcome,
                    final_text,
                },
            ] => {
                assert_eq!(*turn, BatchId(9));
                assert_eq!(*outcome, TurnOutcome::Completed);
                assert_eq!(&**final_text, "hello there!");
            }
            other => panic!("unexpected events: {other:?}"),
        }
        assert!(mapper.handle(&done).is_empty(), "turn already ended");
    }

    #[test]
    fn observe_only_records_our_sessions_tool_arguments() {
        let mut mapper = mapper_for("mine");
        let frame = |session: &str| SseFrame {
            event: None,
            data: format!(
                r#"{{"type":"session.tool.input.ended","data":{{"sessionID":"{session}","id":"tool-call-1","input":{{"command":"echo OK"}}}}}}"#
            ),
        };
        mapper.observe(&frame("theirs"));
        assert!(mapper.tool_arguments.is_empty(), "foreign tool args leaked");
        mapper.observe(&SseFrame {
            event: None,
            data: r#"{"type":"session.tool.input.ended","data":{"id":"tool-call-2","input":{}}}"#
                .into(),
        });
        assert!(
            mapper.tool_arguments.is_empty(),
            "session-less tool args leaked"
        );
        mapper.observe(&frame("mine"));
        assert_eq!(
            mapper.tool_arguments.get("tool-call-1"),
            Some(&serde_json::json!({"command": "echo OK"}))
        );
    }

    #[test]
    fn session_less_events_do_not_advance_the_turn() {
        let mut mapper = mapper_for("mine");
        mapper.begin(BatchId(7));
        let frame = |data: &str| SseFrame {
            event: None,
            data: data.into(),
        };
        for data in [
            r#"{"type":"session.execution.succeeded","data":{}}"#,
            r#"{"type":"session.execution.succeeded"}"#,
            r#"{"type":"session.execution.succeeded","data":{"sessionID":"theirs"}}"#,
        ] {
            assert!(mapper.handle(&frame(data)).is_empty(), "data: {data}");
        }
        assert!(matches!(
            mapper
                .handle(&frame(
                    r#"{"type":"session.execution.succeeded","data":{"sessionID":"mine"}}"#
                ))
                .as_slice(),
            [AgentEvent::TurnEnded { turn, .. }] if *turn == BatchId(7)
        ));
    }

    #[test]
    fn foreign_text_deltas_do_not_enter_our_turn() {
        let mut mapper = mapper_for("mine");
        mapper.begin(BatchId(8));
        let delta = |session: &str, text: &str| SseFrame {
            event: None,
            data: format!(
                r#"{{"type":"session.text.delta","data":{{"sessionID":"{session}","delta":"{text}"}}}}"#
            ),
        };
        assert!(mapper.handle(&delta("theirs", "leak")).is_empty());
        assert!(mapper.handle(&delta("mine", "ours")).is_empty());
        let done = SseFrame {
            event: None,
            data: r#"{"type":"session.execution.succeeded","data":{"sessionID":"mine"}}"#.into(),
        };
        match mapper.handle(&done).as_slice() {
            [AgentEvent::TurnEnded { final_text, .. }] => assert_eq!(&**final_text, "ours"),
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn other_sessions_are_ignored() {
        let mut mapper = EventMapper {
            session: Some("mine".into()),
            ..EventMapper::default()
        };
        mapper.begin(BatchId(4));
        let done = |session: &str| SseFrame {
            event: None,
            data: format!(
                r#"{{"type":"session.execution.succeeded","data":{{"sessionID":"{session}"}}}}"#
            ),
        };
        assert!(mapper.handle(&done("theirs")).is_empty());
        assert!(matches!(
            mapper.handle(&done("mine")).as_slice(),
            [AgentEvent::TurnEnded { turn, .. }] if *turn == BatchId(4)
        ));
    }

    #[test]
    fn failure_fails_the_turn_and_later_success_is_ignored() {
        let mut mapper = mapper_for("s1");
        mapper.begin(BatchId(3));
        let failed = SseFrame {
            event: None,
            data: r#"{"type":"session.execution.failed","data":{"sessionID":"s1","error":{"message":"boom"}}}"#
                .into(),
        };
        assert!(matches!(
            mapper.handle(&failed).as_slice(),
            [AgentEvent::TurnFailed { turn, .. }] if *turn == BatchId(3)
        ));
        let done = SseFrame {
            event: None,
            data: r#"{"type":"session.execution.succeeded","data":{"sessionID":"s1"}}"#.into(),
        };
        assert!(mapper.handle(&done).is_empty());
    }

    #[test]
    fn interruption_is_a_cancelled_turn() {
        let mut mapper = mapper_for("s1");
        mapper.begin(BatchId(5));
        let interrupted = SseFrame {
            event: None,
            data: r#"{"type":"session.execution.interrupted","data":{"sessionID":"s1"}}"#.into(),
        };
        assert!(matches!(
            mapper.handle(&interrupted).as_slice(),
            [AgentEvent::TurnEnded {
                outcome: TurnOutcome::Cancelled,
                ..
            }]
        ));
    }

    #[test]
    fn unknown_and_malformed_events_are_ignored() {
        let mut mapper = mapper_for("s1");
        mapper.begin(BatchId(1));
        for data in [r#"{"type":"session.updated"}"#, "not json", "{}"] {
            let frame = SseFrame {
                event: None,
                data: data.into(),
            };
            assert!(mapper.handle(&frame).is_empty(), "data: {data}");
        }
    }

    #[test]
    fn parses_model_list_from_api_response() {
        let models = parse_model_list(serde_json::json!({
            "location": {"directory": "/tmp"},
            "data": [
                {
                    "id": "anthropic/claude-3-7-sonnet",
                    "modelID": "claude-3-7-sonnet",
                    "providerID": "anthropic",
                    "name": "Claude 3.7 Sonnet"
                },
                {
                    "id": "openai/gpt-4.1",
                    "modelID": "gpt-4.1",
                    "providerID": "openai",
                    "name": "GPT-4.1"
                }
            ]
        }));
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0].id.as_deref(),
            Some("opencode/anthropic/claude-3-7-sonnet")
        );
        assert_eq!(&*models[0].name, "Claude 3.7 Sonnet");
        assert_eq!(models[1].id.as_deref(), Some("opencode/openai/gpt-4.1"));
        assert_eq!(&*models[1].name, "GPT-4.1");
    }

    #[test]
    fn parses_model_list_from_bare_array() {
        let models = parse_model_list(serde_json::json!([
            {
                "id": "opencode/gpt-5",
                "name": "GPT-5"
            }
        ]));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id.as_deref(), Some("opencode/gpt-5"));
        assert_eq!(&*models[0].name, "GPT-5");
    }

    #[test]
    fn opencode_provider_models_use_single_prefix() {
        let info = model_info_from_value(&serde_json::json!({
            "modelID": "big-pickle",
            "providerID": "opencode",
            "name": "Big Pickle"
        }))
        .expect("model");
        assert_eq!(info.id.as_deref(), Some("opencode/big-pickle"));
        assert_eq!(&*info.name, "Big Pickle");
    }

    #[test]
    fn model_without_name_is_ignored() {
        assert!(
            model_info_from_value(&serde_json::json!({
                "id": "anthropic/claude-3-7-sonnet",
                "modelID": "claude-3-7-sonnet",
                "providerID": "anthropic"
            }))
            .is_none()
        );
    }

    #[test]
    fn duplicate_model_names_in_same_provider_are_disambiguated() {
        let models = parse_model_list(serde_json::json!([
            {
                "modelID": "gpt-4o",
                "providerID": "openai",
                "name": "GPT-4o"
            },
            {
                "modelID": "gpt-4o-latest",
                "providerID": "openai",
                "name": "GPT-4o"
            },
            {
                "modelID": "gpt-4o",
                "providerID": "azure",
                "name": "GPT-4o"
            }
        ]));
        assert_eq!(models.len(), 3);
        assert_eq!(&*models[0].name, "GPT-4o");
        assert_eq!(&*models[1].name, "GPT-4o :2");
        assert_eq!(&*models[2].name, "GPT-4o");
    }

    /// End-to-end over loopback: session create, a delivered prompt, and the
    /// event stream feeding `TurnEnded` back into the sink.
    #[tokio::test]
    async fn fake_server_round_trip() {
        use axum::{
            Json, Router,
            routing::{get, post},
        };
        use std::sync::Arc;

        let prompted = Arc::new(tokio::sync::Notify::new());
        let sse_ready = prompted.clone();
        let app = Router::new()
            .route(
                "/api/session",
                post(|| async { Json(serde_json::json!({ "data": { "id": "s1" } })) }),
            )
            .route(
                "/api/session/s1/prompt",
                post({
                    let prompted = prompted.clone();
                    move || {
                        let prompted = prompted.clone();
                        async move {
                            prompted.notify_one();
                            Json(serde_json::json!({ "data": {} }))
                        }
                    }
                }),
            )
            .route(
                "/api/event",
                get(move || {
                    let prompted = sse_ready.clone();
                    async move {
                        prompted.notified().await;
                        let body = concat!(
                            "data: {\"type\":\"session.text.delta\",\"data\":",
                            "{\"sessionID\":\"s1\",\"delta\":\"done\"}}\n\n",
                            "data: {\"type\":\"session.execution.succeeded\",\"data\":",
                            "{\"sessionID\":\"s1\"}}\n\n",
                        );
                        ([("content-type", "text/event-stream")], body)
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let adapter = OpenCodeV2Adapter::new(OpenCodeV2Config {
            command: "opencode".into(),
            args: Vec::new(),
            env: Vec::new(),
            server_url: Some(format!("http://{addr}")),
            server_password: None,
            model: None,
        });
        let session = adapter
            .open(OpenSession {
                agent: crate::types::MemberId(0),
                channel: crate::types::ChannelId(0),
                handle: "tester".into(),
                cwd: std::env::temp_dir(),
                chat_tools: crate::adapter::ChatToolsEndpoint {
                    url: String::new(),
                    token: String::new(),
                    bridge: None,
                },
                events: EventSink::new(move |event| {
                    let _ = tx.send(event);
                }),
                approval_broker: None,
            })
            .await
            .expect("open");
        assert_eq!(&*session.info().native_ref, "s1");

        session
            .deliver(BatchId(1), "hi".into())
            .await
            .expect("deliver");
        let mut ended = None;
        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await
        {
            if let AgentEvent::TurnEnded {
                turn, final_text, ..
            } = event
            {
                ended = Some((turn, final_text));
                break;
            }
        }
        let (turn, text) = ended.expect("turn ended");
        assert_eq!(turn, BatchId(1));
        assert_eq!(&*text, "done");
        session.close().await;
    }

    /// Broker that parks requests until `cancel_pending` answers them, so the
    /// close/interrupt/open-failure paths can be observed end to end.
    struct TestBroker {
        state: std::sync::Arc<std::sync::Mutex<TestBrokerState>>,
    }

    impl Default for TestBroker {
        fn default() -> Self {
            TestBroker {
                state: std::sync::Arc::new(std::sync::Mutex::new(TestBrokerState::default())),
            }
        }
    }

    #[derive(Default)]
    struct TestBrokerState {
        pending:
            HashMap<ApprovalId, tokio::sync::oneshot::Sender<crate::approval::ApprovalDecision>>,
        cancels: Vec<ApprovalResolution>,
        cancel_scopes: Vec<ApprovalScope>,
    }

    impl TestBroker {
        fn park(
            state: std::sync::Arc<std::sync::Mutex<TestBrokerState>>,
            request: ApprovalRequest,
        ) -> BoxFuture<
            'static,
            Result<crate::approval::ApprovalDecision, crate::approval::ApprovalBrokerError>,
        > {
            Box::pin(async move {
                let (tx, rx) = tokio::sync::oneshot::channel();
                {
                    let mut state = state.lock().unwrap();
                    if state.pending.contains_key(&request.cc_request_id) {
                        return Err(crate::approval::ApprovalBrokerError::new("already pending"));
                    }
                    state.pending.insert(request.cc_request_id.clone(), tx);
                }
                let decision = rx.await.map_err(|_| {
                    crate::approval::ApprovalBrokerError::new("parked request abandoned")
                })?;
                state.lock().unwrap().pending.remove(&request.cc_request_id);
                Ok(decision)
            })
        }

        fn cancels(&self) -> Vec<ApprovalResolution> {
            self.state.lock().unwrap().cancels.clone()
        }

        fn cancel_scopes(&self) -> Vec<ApprovalScope> {
            self.state.lock().unwrap().cancel_scopes.clone()
        }

        fn drain_all(
            state: std::sync::Arc<std::sync::Mutex<TestBrokerState>>,
            resolution: ApprovalResolution,
        ) -> Result<(), crate::approval::ApprovalBrokerError> {
            let parked: Vec<_> = {
                let mut state = state.lock().unwrap();
                state.cancels.push(resolution);
                state.pending.drain().collect()
            };
            for (id, tx) in parked {
                let _ = tx.send(crate::approval::ApprovalDecision {
                    cc_request_id: id,
                    decision: ApprovalDecisionKind::NotResolved { resolution },
                    decided_at: 1,
                    actor: None,
                });
            }
            Ok(())
        }
    }

    impl ApprovalBroker for TestBroker {
        fn request_decision(
            &self,
            request: ApprovalRequest,
        ) -> BoxFuture<
            '_,
            Result<crate::approval::ApprovalDecision, crate::approval::ApprovalBrokerError>,
        > {
            TestBroker::park(self.state.clone(), request)
        }

        fn record_outcome(
            &self,
            _outcome: ApprovalOutcome,
        ) -> BoxFuture<'_, Result<(), crate::approval::ApprovalBrokerError>> {
            Box::pin(async { Ok(()) })
        }

        fn cancel_pending(
            &self,
            resolution: ApprovalResolution,
        ) -> BoxFuture<'_, Result<(), crate::approval::ApprovalBrokerError>> {
            let state = self.state.clone();
            Box::pin(async move { TestBroker::drain_all(state, resolution) })
        }

        fn cancel_pending_for(
            &self,
            scope: ApprovalScope,
            resolution: ApprovalResolution,
        ) -> BoxFuture<'_, Result<(), crate::approval::ApprovalBrokerError>> {
            let state = self.state.clone();
            Box::pin(async move {
                state.lock().unwrap().cancel_scopes.push(scope);
                TestBroker::drain_all(state, resolution)
            })
        }
    }

    fn parked_request(id: &str) -> ApprovalRequest {
        ApprovalRequest {
            cc_request_id: ApprovalId::from(id),
            provider: ApprovalProvider::OpenCodeV2,
            provider_request_ref: Arc::from(id),
            session_id: Arc::from("s1"),
            agent: MemberId(0),
            channel: ChannelId(0),
            turn: None,
            operation: ApprovalOperation {
                action: Arc::from("shell"),
                tool_name: None,
                summary: Some(Arc::from("echo OK")),
                arguments: None,
                raw: None,
            },
            choices: ApprovalChoices::DecisionVocabulary {
                decisions: vec![Arc::from("reject"), Arc::from("once"), Arc::from("always")],
                raw: None,
            },
            requested_at: 1,
            context: None,
        }
    }

    async fn wait_for_parked(broker: &TestBroker, id: &ApprovalId) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !broker.state.lock().unwrap().pending.contains_key(id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "parked request {id:?} was not registered"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn open_with_broker(
        server_url: &str,
        broker: std::sync::Arc<TestBroker>,
    ) -> Arc<dyn AgentSession> {
        let approval_broker: Option<Arc<dyn ApprovalBroker>> = Some(broker);
        let adapter = OpenCodeV2Adapter::new(OpenCodeV2Config {
            command: "opencode".into(),
            args: Vec::new(),
            env: Vec::new(),
            server_url: Some(server_url.to_owned()),
            server_password: None,
            model: None,
        });
        adapter
            .open(OpenSession {
                agent: MemberId(0),
                channel: ChannelId(0),
                handle: "tester".into(),
                cwd: std::env::temp_dir(),
                chat_tools: crate::adapter::ChatToolsEndpoint {
                    url: String::new(),
                    token: String::new(),
                    bridge: None,
                },
                events: EventSink::new(|_| {}),
                approval_broker,
            })
            .await
            .expect("open")
    }

    #[tokio::test]
    async fn close_cancels_pending_approvals() {
        use axum::{
            Json, Router,
            routing::{get, post},
        };

        let app = Router::new()
            .route(
                "/api/session",
                post(|| async { Json(serde_json::json!({ "data": { "id": "s1" } })) }),
            )
            .route(
                "/api/event",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    ([("content-type", "text/event-stream")], "")
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let broker = Arc::new(TestBroker::default());
        let session = open_with_broker(&format!("http://{addr}"), broker.clone()).await;

        let parked_id = ApprovalId::from("opencode-v2:s1:req-close");
        let parked = tokio::spawn(TestBroker::park(
            broker.state.clone(),
            parked_request(parked_id.as_str()),
        ));
        wait_for_parked(&broker, &parked_id).await;

        session.close().await;

        let decision = parked.await.unwrap().unwrap();
        assert!(matches!(
            decision.decision,
            crate::approval::ApprovalDecisionKind::NotResolved {
                resolution: ApprovalResolution::Disconnect
            }
        ));
        assert_eq!(broker.cancels(), vec![ApprovalResolution::Disconnect]);
        assert_eq!(
            broker.cancel_scopes(),
            vec![ApprovalScope::new(
                ApprovalProvider::OpenCodeV2,
                Arc::from("s1")
            )]
        );
    }

    #[tokio::test]
    async fn interrupt_cancels_pending_approvals() {
        use axum::{
            Json, Router,
            routing::{get, post},
        };

        let app = Router::new()
            .route(
                "/api/session",
                post(|| async { Json(serde_json::json!({ "data": { "id": "s1" } })) }),
            )
            .route(
                "/api/session/s1/interrupt",
                post(|| async { Json(serde_json::json!({ "data": {} })) }),
            )
            .route(
                "/api/event",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    ([("content-type", "text/event-stream")], "")
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let broker = Arc::new(TestBroker::default());
        let session = open_with_broker(&format!("http://{addr}"), broker.clone()).await;

        let parked_id = ApprovalId::from("opencode-v2:s1:req-interrupt");
        let parked = tokio::spawn(TestBroker::park(
            broker.state.clone(),
            parked_request(parked_id.as_str()),
        ));
        wait_for_parked(&broker, &parked_id).await;

        session.interrupt().await.unwrap();

        let decision = parked.await.unwrap().unwrap();
        assert!(matches!(
            decision.decision,
            crate::approval::ApprovalDecisionKind::NotResolved {
                resolution: ApprovalResolution::Disconnect
            }
        ));
        assert_eq!(broker.cancels(), vec![ApprovalResolution::Disconnect]);
    }

    #[tokio::test]
    async fn event_stream_open_failure_cancels_pending_approvals() {
        use axum::{Json, Router, routing::post};

        let app = Router::new().route(
            "/api/session",
            post(|| async { Json(serde_json::json!({ "data": { "id": "s1" } })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let broker = Arc::new(TestBroker::default());
        open_with_broker(&format!("http://{addr}"), broker.clone()).await;

        // `/api/event` is absent: the stream open fails, and the failure must
        // fail pending approvals closed.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while broker.cancels().is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no cancel recorded for stream-open failure"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(broker.cancels(), vec![ApprovalResolution::Disconnect]);
    }
}
