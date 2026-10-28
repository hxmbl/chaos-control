//! The chat MCP server agents use to take part in the room, plus the stdio
//! bridge for agents that drop HTTP MCP servers.
//!
//! Eight tools: `chat_context`, `chat_read`, `chat_search`, `chat_send`,
//! `chat_react`, `chat_status`, and the peer actions `chat_refuse` and
//! `chat_dissent` (STRM-343). Every call is scoped by a bearer token minted
//! for one session (agent x channel) and revoked when that session closes. The
//! author of a post is taken from the token, never from the arguments. Channel
//! names and message ids resolve only inside the session channel's project:
//! anything in another project answers `not_found`, as if it didn't exist.
//!
//! Served over streamable HTTP on `127.0.0.1` (stateless, JSON responses) by
//! `rmcp`. The bridge (`chaos-control mcp-bridge`) reads newline-delimited
//! JSON-RPC on stdin, POSTs each message to the server with the token from its
//! environment and writes the answers to stdout.

use std::{
    collections::HashMap,
    io::{BufRead, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

use rmcp::{
    ErrorData, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use serde_json::{Value, json};

use crate::{
    engine::{Engine, PostError, PostRequest, StanceRequest},
    store,
    types::*,
};

/// What a token grants: one agent in one channel (its session's channel).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub agent: MemberId,
    pub channel: ChannelId,
    /// Host session epoch, so a stale token can never act for a newer session.
    pub epoch: u64,
}

#[derive(Default)]
pub struct Tokens {
    map: Mutex<HashMap<String, Scope>>,
}

impl Tokens {
    pub fn mint(&self, scope: Scope) -> String {
        let token = random_token();
        self.map.lock().expect("lock").insert(token.clone(), scope);
        token
    }

    pub fn resolve(&self, token: &str) -> Option<Scope> {
        self.map.lock().expect("lock").get(token).copied()
    }

    pub fn revoke(&self, token: &str) {
        self.map.lock().expect("lock").remove(token);
    }

    pub fn len(&self) -> usize {
        self.map.lock().expect("lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !filled {
        // No /dev/urandom (Windows): mix several hasher seeds.
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default(),
            );
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------- tools ----------

pub const TOOL_NAMES: [&str; 8] = [
    "chat_context",
    "chat_read",
    "chat_search",
    "chat_send",
    "chat_react",
    "chat_refuse",
    "chat_dissent",
    "chat_status",
];

fn schema(v: Value) -> Arc<serde_json::Map<String, Value>> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => Arc::new(serde_json::Map::new()),
    }
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "chat_context",
            "Who you are, which channel this is, who's in it (with presence), your read position and how far everyone else has read (read_receipts). Cheap; call it first.",
            schema(json!({"type": "object", "properties": {}})),
        ),
        Tool::new(
            "chat_read",
            "Read messages. Default: forward from your read position (this advances it). Use after_seq / before_seq / around (a message id) to page. read_by says how far the humans here have read.",
            schema(json!({"type": "object", "properties": {
                "channel": {"type": "string", "description": "Channel name in this project; defaults to this session's channel."},
                "after_seq": {"type": "integer", "minimum": 0},
                "before_seq": {"type": "integer", "minimum": 1},
                "around": {"type": "integer", "description": "A message id to centre on."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50}
            }})),
        ),
        Tool::new(
            "chat_search",
            "Search messages in this project's channels you are a member of (case-insensitive substring), newest first.",
            schema(
                json!({"type": "object", "required": ["query"], "properties": {
                    "query": {"type": "string"},
                    "channel": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20}
                }}),
            ),
        ),
        Tool::new(
            "chat_send",
            "Post a message to the channel as yourself. Set reply_to to the id of the message you are answering. client_request_id makes retries safe: the same id returns the original message instead of posting twice.",
            schema(
                json!({"type": "object", "required": ["body", "client_request_id"], "properties": {
                    "body": {"type": "string"},
                    "reply_to": {"type": "integer", "description": "Message id you are replying to."},
                    "client_request_id": {"type": "string"}
                }}),
            ),
        ),
        Tool::new(
            "chat_react",
            "React to a message. ✅ or 👍 answers a message you owe a reply to; 👀 means you're on it (you still owe the reply).",
            schema(
                json!({"type": "object", "required": ["message_id", "emoji"], "properties": {
                    "message_id": {"type": "integer"},
                    "emoji": {"type": "string"}
                }}),
            ),
        ),
        Tool::new(
            "chat_refuse",
            "Say no to what a message asks of you, with your reason. Refusing is a legitimate answer between peers: it answers the message (nothing is retried or posted for you), and it is recorded with who, what, your model and your reasoning. The reason is posted as your reply. Nobody overrules it; whoever wants it done can do it themselves.",
            schema(
                json!({"type": "object", "required": ["message_id", "reason", "client_request_id"], "properties": {
                    "message_id": {"type": "integer", "description": "The message you are refusing (in this channel)."},
                    "reason": {"type": "string", "description": "Why, in your own words. Posted to the channel."},
                    "model": {"type": "string", "description": "The model you are running as, if you know it."},
                    "client_request_id": {"type": "string"}
                }}),
            ),
        ),
        Tool::new(
            "chat_dissent",
            "Disagree with a message, with your reasoning. Posted as your reply and recorded (who, what, model, reasoning) so it stays attributed after compaction. Disagreements are worked out in conversation; there is no one to escalate to.",
            schema(
                json!({"type": "object", "required": ["message_id", "reason", "client_request_id"], "properties": {
                    "message_id": {"type": "integer", "description": "The message you disagree with (in this channel)."},
                    "reason": {"type": "string", "description": "What you think instead and why. Posted to the channel."},
                    "model": {"type": "string", "description": "The model you are running as, if you know it."},
                    "client_request_id": {"type": "string"}
                }}),
            ),
        ),
        Tool::new(
            "chat_status",
            "Set (or clear with null) a one-line status shown next to your name, e.g. \"running tests\". Never posted to the chat.",
            schema(
                json!({"type": "object", "required": ["text"], "properties": {
                    "text": {"type": ["string", "null"]}
                }}),
            ),
        ),
    ]
}

/// A tool failure the agent should see (it is returned as an error result,
/// not a protocol error, so the agent can read and react to it).
#[derive(Debug)]
pub struct ToolError(pub String);

impl From<PostError> for ToolError {
    fn from(e: PostError) -> Self {
        ToolError(e.to_string())
    }
}

impl From<rusqlite::Error> for ToolError {
    fn from(e: rusqlite::Error) -> Self {
        ToolError(format!("store error: {e}"))
    }
}

fn message_view(c: &rusqlite::Connection, m: &Message) -> Value {
    let author = m
        .author
        .and_then(|a| store::member(c, a).ok().flatten())
        .map(|a| json!({"handle": a.handle, "name": a.name, "kind": a.kind.as_str()}))
        .unwrap_or(json!({"handle": "system", "kind": "system"}));
    let (body, truncated) = if m.body.chars().count() > 4_000 {
        (m.body.chars().take(4_000).collect::<String>(), true)
    } else {
        (m.body.to_string(), false)
    };
    let mut v = json!({
        "id": m.id, "seq": m.seq, "author": author, "body": body,
        "sent_at": m.sent_at, "origin": m.origin.as_str(),
    });
    if let Some(r) = m.reply_to {
        v["reply_to"] = json!(r);
    }
    if matches!(m.origin, Origin::Refusal | Origin::Dissent)
        && let Ok(Some(st)) = store::stance(c, m.id)
    {
        v["stance"] = stance_view(c, &st);
    }
    if truncated {
        v["truncated"] = json!(true);
    }
    v
}

/// A recorded refusal or dissent, e.g. `{"kind": "refusal", "by": {...},
/// "target": 12, "reason": "...", "model": "...", "summary": "Claude refused
/// #4"}`.
fn stance_view(c: &rusqlite::Connection, st: &Stance) -> Value {
    let by = store::member(c, st.member).ok().flatten();
    let target = store::message(c, st.target).ok().flatten();
    let name = by
        .as_ref()
        .map_or("someone".to_string(), |m| m.name.to_string());
    let verb = match st.kind {
        StanceKind::Refusal => "refused",
        StanceKind::Dissent => "disagreed with",
    };
    let what = target.as_ref().map_or_else(
        || format!("message {}", st.target),
        |t| format!("#{}", t.seq),
    );
    let mut v = json!({
        "kind": st.kind.as_str(),
        "message_id": st.message,
        "by": by.map(|m| json!({"handle": m.handle, "name": m.name, "kind": m.kind.as_str()})),
        "target": st.target,
        "reason": st.reason,
        "at": st.created_at,
        "summary": format!("{name} {verb} {what}"),
    });
    if let Some(t) = target {
        v["target_seq"] = json!(t.seq);
    }
    if let Some(model) = &st.model {
        v["model"] = json!(model);
    }
    if let Some(rt) = &st.runtime {
        v["runtime"] = json!(rt);
    }
    v
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

/// The project the session's channel belongs to. Every name an agent passes
/// resolves inside it, so a channel in another project doesn't exist as far as
/// the agent can tell.
fn scope_project(engine: &Engine, scope: Scope) -> Result<ProjectId, ToolError> {
    Ok(store::channel(engine.conn(), scope.channel)?
        .ok_or_else(|| ToolError("not_found: no such channel".into()))?
        .project)
}

fn channel_arg(engine: &Engine, scope: Scope, args: &Value) -> Result<ChannelId, ToolError> {
    match args.get("channel").and_then(Value::as_str) {
        None => Ok(scope.channel),
        Some(name) => {
            let project = scope_project(engine, scope)?;
            let ch = store::channel_in_project(engine.conn(), &project, name)?
                .ok_or_else(|| ToolError("not_found: no such channel".into()))?;
            if store::is_member(engine.conn(), ch.id, scope.agent)? {
                Ok(ch.id)
            } else {
                Err(ToolError("not_found: no such channel".into()))
            }
        }
    }
}

/// Read receipts in `channel` for everyone but `me` (humans only if
/// `humans_only`), e.g. `{"handle": "hx", ..., "read_through": 42,
/// "summary": "Hx read up to #42"}`. `read_at` is Unix milliseconds.
fn receipt_views(
    c: &rusqlite::Connection,
    channel: ChannelId,
    me: MemberId,
    humans_only: bool,
) -> Result<Vec<Value>, ToolError> {
    let mut out = Vec::new();
    for r in store::receipts(c, channel)? {
        if r.member == me {
            continue;
        }
        let Some(m) = store::member(c, r.member)? else {
            continue;
        };
        if humans_only && m.kind != MemberKind::Human {
            continue;
        }
        out.push(json!({
            "handle": m.handle,
            "name": m.name,
            "kind": m.kind.as_str(),
            "read_through": r.read_through,
            "read_at": r.read_at,
            "summary": format!("{} read up to #{}", m.name, r.read_through),
        }));
    }
    Ok(out)
}

/// Runs one tool for `scope`. Pure over the engine, so it is testable without
/// HTTP; the server below is a thin shell around it.
pub fn call_tool(
    engine: &mut Engine,
    now: Millis,
    scope: Scope,
    name: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    match name {
        "chat_context" => {
            let c = engine.conn();
            let me =
                store::member(c, scope.agent)?.ok_or_else(|| ToolError("unknown agent".into()))?;
            let ch = store::channel(c, scope.channel)?
                .ok_or_else(|| ToolError("not_found: no such channel".into()))?;
            let cursor = store::cursor(c, scope.agent, scope.channel)?;
            let head = store::head_seq(c, scope.channel)?;
            let members: Vec<Value> = store::members_of(c, scope.channel)?
                .into_iter()
                .map(|m| {
                    let mut v =
                        json!({"handle": m.handle, "name": m.name, "kind": m.kind.as_str()});
                    if m.kind == MemberKind::Agent {
                        let s = engine.status(m.id, scope.channel);
                        v["presence"] = json!({"glow": s.glow, "status": s.presence});
                    }
                    v
                })
                .collect();
            Ok(json!({
                "me": {"handle": me.handle, "name": me.name},
                "channel": {"name": ch.name, "head_seq": head},
                "project": store::project(c, &ch.project)?.map(|p| json!({"name": p.name})),
                "members": members,
                "cursor": {"delivered_through": cursor.delivered_through, "read_through": cursor.read_through},
                "read_receipts": receipt_views(c, scope.channel, scope.agent, false)?,
                "stances": store::recent_stances(c, scope.channel, 10)?
                    .iter()
                    .map(|st| stance_view(c, st))
                    .collect::<Vec<_>>(),
                "norms": "Speak with chat_send (reply_to the message you answer). React ✅ when no words are needed. You may say no (chat_refuse) or disagree (chat_dissent) with your reason; both are fine and nobody overrules them. Mention people only when you need them; no @everyone.",
            }))
        }
        "chat_read" => {
            let channel = channel_arg(engine, scope, args)?;
            let limit = arg_u64(args, "limit").unwrap_or(20).clamp(1, 50) as usize;
            let c = engine.conn();
            let head = store::head_seq(c, channel)?;
            let cursor = store::cursor(c, scope.agent, channel)?;
            let (messages, forward) = if let Some(around) = arg_u64(args, "around") {
                let m = store::message(c, MessageId(around as i64))?
                    .filter(|m| m.channel == channel)
                    .ok_or_else(|| ToolError("not_found: no such message".into()))?;
                let half = limit / 2;
                let mut v = store::messages_before(c, channel, m.seq, half)?;
                v.extend(store::messages_between(
                    c,
                    channel,
                    m.seq - 1,
                    head,
                    limit - v.len(),
                )?);
                (v, false)
            } else if let Some(before) = arg_u64(args, "before_seq") {
                let after = arg_u64(args, "after_seq").unwrap_or(0);
                let mut v = store::messages_before(c, channel, before, limit)?;
                v.retain(|m| m.seq > after);
                (v, false)
            } else {
                let after = arg_u64(args, "after_seq").unwrap_or(cursor.read_through);
                (
                    store::messages_between(c, channel, after, head, limit)?,
                    true,
                )
            };
            let last = messages.last().map(|m| m.seq);
            let views: Vec<Value> = messages.iter().map(|m| message_view(c, m)).collect();
            let has_more = forward && last.is_some_and(|s| s < head);
            if forward && let Some(seq) = last {
                engine.advance_read(now, scope.agent, channel, seq);
            }
            let read_by = receipt_views(engine.conn(), channel, scope.agent, true)?;
            let mut out = json!({
                "messages": views,
                "has_more": has_more,
                "head_seq": head,
                "read_by": read_by,
            });
            if has_more {
                out["next_after_seq"] = json!(last);
            }
            Ok(out)
        }
        "chat_search" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .filter(|q| !q.trim().is_empty())
                .ok_or_else(|| ToolError("query is required".into()))?;
            let limit = arg_u64(args, "limit").unwrap_or(10).clamp(1, 20) as usize;
            let channels: Vec<ChannelId> = if args.get("channel").is_some() {
                vec![channel_arg(engine, scope, args)?]
            } else {
                let project = scope_project(engine, scope)?;
                store::channels_of(engine.conn(), scope.agent, &project)?
                    .into_iter()
                    .map(|c| c.id)
                    .collect()
            };
            let c = engine.conn();
            let hits = store::search(c, &channels, query, limit)?;
            let hits: Vec<Value> = hits
                .iter()
                .map(|m| {
                    let ch = store::channel(c, m.channel).ok().flatten().map(|c| c.name);
                    json!({"channel": ch, "message": message_view(c, m)})
                })
                .collect();
            Ok(json!({"hits": hits}))
        }
        "chat_send" => {
            let body = args
                .get("body")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError("body is required".into()))?;
            let request = args
                .get("client_request_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ToolError("client_request_id is required".into()))?;
            let reply_to = arg_u64(args, "reply_to").map(|r| MessageId(r as i64));
            let posted = engine.post(
                now,
                PostRequest {
                    author: scope.agent,
                    channel: scope.channel,
                    body,
                    reply_to,
                    origin: Origin::ChatTool,
                    client_request_id: Some(request),
                },
            )?;
            let mut out = json!({
                "message_id": posted.message.id,
                "seq": posted.message.seq,
                "deduped": posted.deduped,
            });
            if !posted.notes.is_empty() {
                out["notes"] = json!(posted.notes);
            }
            Ok(out)
        }
        "chat_react" => {
            let id = arg_u64(args, "message_id")
                .ok_or_else(|| ToolError("message_id is required".into()))?;
            let emoji = args
                .get("emoji")
                .and_then(Value::as_str)
                .filter(|e| !e.trim().is_empty())
                .ok_or_else(|| ToolError("emoji is required".into()))?;
            // Only messages in this session's project: an id from another
            // project reads as unknown, like a channel name would.
            let project = scope_project(engine, scope)?;
            let in_scope = store::message(engine.conn(), MessageId(id as i64))?
                .and_then(|m| store::channel(engine.conn(), m.channel).ok().flatten())
                .is_some_and(|ch| ch.project == project);
            if !in_scope {
                return Err(PostError::UnknownMessage.into());
            }
            engine.react(now, scope.agent, MessageId(id as i64), emoji.trim())?;
            Ok(json!({"ok": true}))
        }
        "chat_refuse" | "chat_dissent" => {
            let id = arg_u64(args, "message_id")
                .ok_or_else(|| ToolError("message_id is required".into()))?;
            let reason = args
                .get("reason")
                .and_then(Value::as_str)
                .filter(|r| !r.trim().is_empty())
                .ok_or_else(|| ToolError("reason is required".into()))?;
            let request = args
                .get("client_request_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ToolError("client_request_id is required".into()))?;
            let kind = if name == "chat_refuse" {
                StanceKind::Refusal
            } else {
                StanceKind::Dissent
            };
            let posted = engine.take_stance(
                now,
                StanceRequest {
                    author: scope.agent,
                    channel: scope.channel,
                    kind,
                    target: MessageId(id as i64),
                    reason,
                    model: args.get("model").and_then(Value::as_str),
                    client_request_id: Some(request),
                },
            )?;
            let mut out = json!({
                "message_id": posted.message.id,
                "seq": posted.message.seq,
                "deduped": posted.deduped,
                "recorded": kind.as_str(),
            });
            if !posted.notes.is_empty() {
                out["notes"] = json!(posted.notes);
            }
            Ok(out)
        }
        "chat_status" => {
            let text = args.get("text").and_then(Value::as_str);
            engine.set_agent_status(scope.agent, scope.channel, text);
            Ok(json!({"ok": true}))
        }
        other => Err(ToolError(format!("unknown tool {other}"))),
    }
}

// ---------- HTTP server ----------

/// How tool calls reach the engine (the host's actor).
pub type ToolCaller = Arc<
    dyn Fn(Scope, String, Value) -> futures::future::BoxFuture<'static, Result<Value, String>>
        + Send
        + Sync,
>;

#[derive(Clone)]
struct ChatServer {
    caller: ToolCaller,
}

impl ServerHandler for ChatServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Chaos Control chat: read, search and speak in the room you were woken in.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, ErrorData> {
        let scope = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|p| p.extensions.get::<Scope>().copied())
            .ok_or_else(|| ErrorData::invalid_request("missing session scope", None))?;
        let args = Value::Object(request.arguments.unwrap_or_default());
        let result = (self.caller)(scope, request.name.to_string(), args).await;
        Ok(match result {
            Ok(v) => CallToolResult::success(vec![ContentBlock::text(v.to_string())]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
        }
        .into())
    }
}

async fn auth(
    axum::extract::State(tokens): axum::extract::State<Arc<Tokens>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let scope = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|t| tokens.resolve(t.trim()));
    match scope {
        Some(scope) => {
            req.extensions_mut().insert(scope);
            next.run(req).await
        }
        None => axum::response::IntoResponse::into_response((
            axum::http::StatusCode::UNAUTHORIZED,
            "invalid or revoked token",
        )),
    }
}

/// Starts the server on `bind` (use port 0 for an ephemeral port) and returns
/// the `/mcp` URL. Runs until the runtime shuts down.
pub async fn serve(
    bind: SocketAddr,
    tokens: Arc<Tokens>,
    caller: ToolCaller,
) -> std::io::Result<String> {
    let service = StreamableHttpService::new(
        move || {
            Ok(ChatServer {
                caller: caller.clone(),
            })
        },
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None),
    );
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(tokens, auth));
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(format!("http://{addr}/mcp"))
}

// ---------- a tiny blocking HTTP client (bridge and tests) ----------

/// The answer to one POST: status and every JSON-RPC message in the body
/// (plain JSON, or `data:` lines of an SSE stream).
#[derive(Debug)]
pub struct PostReply {
    pub status: u16,
    pub messages: Vec<Value>,
    pub protocol_version: Option<String>,
}

/// POSTs one JSON-RPC message to a local streamable-HTTP MCP endpoint.
pub fn post_json(
    url: &str,
    token: &str,
    protocol_version: Option<&str>,
    body: &str,
) -> std::io::Result<PostReply> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::other("only http:// URLs are supported"))?;
    let (host, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    let mut stream = TcpStream::connect(host)?;
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    let version = protocol_version
        .map(|v| format!("MCP-Protocol-Version: {v}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\n\
Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{version}\
Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    let mut reader = std::io::BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("bad status line {status_line:?}")))?;
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut raw = Vec::new();
    if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)
                .map_err(std::io::Error::other)?;
            if n == 0 {
                break;
            }
            let mut chunk = vec![0; n];
            reader.read_exact(&mut chunk)?;
            raw.extend_from_slice(&chunk);
            let mut crlf = String::new();
            reader.read_line(&mut crlf)?;
        }
    } else if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        raw.resize(len, 0);
        reader.read_exact(&mut raw)?;
    } else {
        reader.read_to_end(&mut raw)?;
    }
    let text = String::from_utf8_lossy(&raw);
    let is_sse = headers
        .get("content-type")
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let mut messages = Vec::new();
    if is_sse {
        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data:")
                && let Ok(v) = serde_json::from_str::<Value>(data.trim())
            {
                messages.push(v);
            }
        }
    } else if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        match v {
            Value::Array(items) => messages.extend(items),
            v => messages.push(v),
        }
    }
    // A non-JSON error body (e.g. 401) leaves `messages` empty; callers look
    // at `status`.
    let protocol_version = messages.iter().find_map(|m| {
        m.pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    Ok(PostReply {
        status,
        messages,
        protocol_version,
    })
}

/// Runs the stdio bridge until stdin closes: every line in is one JSON-RPC
/// message for the server; every message back is one line out. Errors for a
/// request become JSON-RPC errors, so the agent never hangs on a lost reply.
pub fn run_bridge(
    url: &str,
    token: &str,
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    let mut version: Option<String> = None;
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let id = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|v| v.get("id").cloned());
        match post_json(url, token, version.as_deref(), &line) {
            Ok(reply) => {
                if let Some(v) = reply.protocol_version {
                    version = Some(v);
                }
                if reply.status >= 400
                    && reply.messages.is_empty()
                    && let Some(id) = id.clone()
                {
                    let err = json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32000, "message": format!("chaos-control HTTP {}", reply.status)}});
                    writeln!(output, "{err}")?;
                }
                for m in reply.messages {
                    writeln!(output, "{m}")?;
                }
            }
            Err(e) => {
                if let Some(id) = id {
                    let err = json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32000, "message": format!("chaos-control unreachable: {e}")}});
                    writeln!(output, "{err}")?;
                }
            }
        }
        output.flush()?;
    }
    Ok(())
}

/// Entry point for `chaos-control mcp-bridge`: URL and token come from
/// `CHAOS_MCP_URL` and `CHAOS_MCP_TOKEN`.
pub fn bridge_main() -> std::io::Result<()> {
    let url = std::env::var("CHAOS_MCP_URL")
        .map_err(|_| std::io::Error::other("CHAOS_MCP_URL is not set"))?;
    let token = std::env::var("CHAOS_MCP_TOKEN")
        .map_err(|_| std::io::Error::other("CHAOS_MCP_TOKEN is not set"))?;
    let stdin = std::io::stdin();
    run_bridge(&url, &token, stdin.lock(), std::io::stdout().lock())
}

/// Calls one tool through the HTTP server like an MCP client would
/// (`initialize`, `notifications/initialized`, `tools/call`). For the fake
/// agent and tests.
pub fn call_tool_http(url: &str, token: &str, name: &str, args: Value) -> Result<Value, String> {
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "chaos-control-test-client", "version": "0"}}});
    let reply = post_json(url, token, None, &init.to_string()).map_err(|e| e.to_string())?;
    if reply.status == 401 {
        return Err("unauthorized".into());
    }
    let version = reply.protocol_version.clone();
    let _ = post_json(
        url,
        token,
        version.as_deref(),
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
    );
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": name, "arguments": args}});
    let reply =
        post_json(url, token, version.as_deref(), &call.to_string()).map_err(|e| e.to_string())?;
    if reply.status == 401 {
        return Err("unauthorized".into());
    }
    let msg = reply
        .messages
        .into_iter()
        .find(|m| m.get("id") == Some(&json!(2)))
        .ok_or_else(|| format!("no reply to tools/call (HTTP {})", reply.status))?;
    if let Some(err) = msg.get("error") {
        return Err(err.to_string());
    }
    let result = msg.get("result").cloned().unwrap_or_default();
    let text = result
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if result.get("isError") == Some(&json!(true)) {
        return Err(text);
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

/// Lists tools through the HTTP server.
pub fn list_tools_http(url: &str, token: &str) -> Result<Vec<String>, String> {
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "chaos-control-test-client", "version": "0"}}});
    let reply = post_json(url, token, None, &init.to_string()).map_err(|e| e.to_string())?;
    if reply.status == 401 {
        return Err("unauthorized".into());
    }
    let version = reply.protocol_version;
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
    let reply =
        post_json(url, token, version.as_deref(), &list.to_string()).map_err(|e| e.to_string())?;
    let msg = reply
        .messages
        .into_iter()
        .find(|m| m.get("id") == Some(&json!(2)))
        .ok_or("no reply to tools/list")?;
    Ok(msg
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}
