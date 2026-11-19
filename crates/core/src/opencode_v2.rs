//! Native OpenCode V2 adapter (STRM-612).
//!
//! Connects to an OpenCode v2 server (`opencode serve`) over HTTP/SSE instead
//! of speaking ACP (`opencode acp`). This is intentionally separate from the
//! ACP adapters so existing ACP paths remain intact.
//!
//! Status: initial slice for the STRM-612 ship-today window. Capabilities and
//! enforcement guarantees must be reported honestly; unsupported or unverified
//! behavior must not be presented as protected.

use std::{path::PathBuf, sync::Arc};

use futures::{StreamExt, future::BoxFuture};
use tokio::io::AsyncBufReadExt;

use crate::{
    adapter::{
        AgentAdapter, AgentError, AgentErrorKind, AgentEvent, AgentSession, Caps,
        ChatToolsTransport, EventSink, OpenSession, Probe, SessionInfo, TurnOutcome,
    },
    types::BatchId,
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

struct SessionInner {
    http: Http,
    session_id: Arc<str>,
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
            self.inner
                .http
                .post_json(&path, &serde_json::json!({}))
                .await
                .map(|_| ())
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
        })
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
                fail_in_flight(&inner, &transport_error(e));
                return;
            }
        };
        parser.push(&bytes);
        while let Some(frame) = parser.next_frame() {
            answer_permission(&inner, &frame).await;
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
    inner.events.emit(AgentEvent::Exited {
        error: Some("opencode v2 event stream closed".into()),
    });
}

/// Permissions are not mediated (the model is being redesigned), so a request
/// is answered at once with the narrowest allow: `once`.
async fn answer_permission(inner: &SessionInner, frame: &SseFrame) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&frame.data) else {
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
    let path = format!(
        "/api/session/{}/permission/{request}/reply",
        inner.session_id
    );
    let _ = inner
        .http
        .post_json(&path, &serde_json::json!({ "decision": "once" }))
        .await;
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
/// for other sessions are dropped by `data.sessionID`.
#[derive(Default)]
struct EventMapper {
    turn: Option<BatchId>,
    /// Our native session id, once known; `None` accepts every session.
    session: Option<String>,
    text: String,
}

impl EventMapper {
    fn begin(&mut self, turn: BatchId) {
        self.turn = Some(turn);
        self.text.clear();
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
        let data = value
            .get("data")
            .or_else(|| value.get("properties"))
            .unwrap_or(&value);
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
        match (
            &self.session,
            data.get("sessionID").and_then(|s| s.as_str()),
        ) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => true,
        }
    }
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
            "/api/experimental/mcp/chaos-control",
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
        let (provider_id, model_id) = match model.split_once('/') {
            Some((provider, id)) if !provider.is_empty() && !id.is_empty() => (provider, id),
            _ => ("opencode", model),
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
        mapper: std::sync::Mutex::new(mapper),
        events: req.events.clone(),
        child: tokio::sync::Mutex::new(child),
        sse: tokio::sync::Mutex::new(None),
    });
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
        models: Vec::new(),
        model_note: None,
    };
    Ok(Arc::new(OpenCodeV2Session { info, inner }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut mapper = EventMapper::default();
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
        let mut mapper = EventMapper::default();
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
        let mut mapper = EventMapper::default();
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
        let mut mapper = EventMapper::default();
        mapper.begin(BatchId(1));
        for data in [r#"{"type":"session.updated"}"#, "not json", "{}"] {
            let frame = SseFrame {
                event: None,
                data: data.into(),
            };
            assert!(mapper.handle(&frame).is_empty(), "data: {data}");
        }
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
}
