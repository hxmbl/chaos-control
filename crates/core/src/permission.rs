//! Permissions by where the agent runs (STRM-401). The rules are written up in
//! `docs/agents.md` ("Permissions").
//!
//! - **On the host** (the user's computer) a permission request goes to the
//!   humans and never blocks the agent. The agent hears "request sent" with a
//!   request id at once and decides whether to wait or do something else. A
//!   human's Approve or Deny arrives later, as a message that wakes the agent
//!   (its next turn). Nothing is ever approved automatically and nothing times
//!   out into an approval.
//! - **In the agent's own VM** ([`RunsIn::Vm`]) everything is approved: a broken
//!   VM is replaced, not protected.
//!
//! A protocol whose permission call blocks (ACP `session/request_permission`)
//! is answered at once: with the agent's own allow-once option when the
//! request is approved (VM, or a one-shot grant a human already gave for that
//! exact request), else with its own decline ("not now") option and a `_meta`
//! note that a request was sent. An approval becomes a one-shot grant: when the
//! agent retries the same tool call (same title) in that channel, it goes
//! through once. Agents can also ask ahead, and poll, through the chat tools
//! `chat_request_permission` and `chat_permission_status`.
//!
//! Peer model: a declined or denied request is an answer, not a failure. It
//! never turns an agent red or fails its turn.

use std::{fmt, sync::Arc};

use futures::future::BoxFuture;
use rmcp::model::Tool;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    engine::{CoreEvent, Effect, Engine, PostError},
    mcp::{Scope, ToolError},
    store,
    types::*,
};

// ---------- types ----------

/// Where an agent runs, which decides how its permission requests are handled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunsIn {
    /// The user's computer: requests go to the humans, non-blocking.
    #[default]
    Host,
    /// The agent's own VM: every request is approved.
    Vm,
}

/// A permission request's id. Stable, never reused (`AUTOINCREMENT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PermissionId(pub i64);

impl fmt::Display for PermissionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Where a request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionSource {
    /// The agent's protocol asked (ACP `session/request_permission`).
    Native,
    /// The agent asked ahead with `chat_request_permission`.
    ChatTool,
}

impl PermissionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionSource::Native => "native",
            PermissionSource::ChatTool => "chat_tool",
        }
    }

    pub fn parse(s: &str) -> PermissionSource {
        match s {
            "chat_tool" => PermissionSource::ChatTool,
            _ => PermissionSource::Native,
        }
    }
}

/// Where a request stands. `Pending` only ever moves to `Approved` or
/// `Denied`, by a human; `AutoApproved` is written as such (VM agents).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Pending,
    Approved,
    Denied,
    AutoApproved,
}

impl PermissionState {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionState::Pending => "pending",
            PermissionState::Approved => "approved",
            PermissionState::Denied => "denied",
            PermissionState::AutoApproved => "auto_approved",
        }
    }

    pub fn parse(s: &str) -> PermissionState {
        match s {
            "approved" => PermissionState::Approved,
            "denied" => PermissionState::Denied,
            "auto_approved" => PermissionState::AutoApproved,
            _ => PermissionState::Pending,
        }
    }
}

/// One permission request, as stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PermissionRequest {
    pub id: PermissionId,
    pub agent: MemberId,
    pub channel: ChannelId,
    /// The turn it was asked in, if one was running.
    pub batch: Option<BatchId>,
    pub source: PermissionSource,
    /// What it wants to do: the tool call's title ("rm -rf build/").
    pub title: Arc<str>,
    /// The tool's raw input, when the protocol gave one (display only).
    pub detail: Option<Arc<str>>,
    /// Why, in the agent's words (chat tool only).
    pub reason: Option<Arc<str>>,
    pub state: PermissionState,
    /// The humans it was routed to: whoever woke the agent (else every
    /// human in the channel). Any human member can answer.
    pub asked_of: Vec<MemberId>,
    pub created_at: Millis,
    pub answered_by: Option<MemberId>,
    pub answered_at: Option<Millis>,
    pub answer_note: Option<Arc<str>>,
    /// When an approval was used up by the retried tool call (one-shot grant).
    pub used_at: Option<Millis>,
}

/// A native permission request, as the adapter hands it to core.
#[derive(Clone, Debug)]
pub struct PermissionAsk {
    pub turn: Option<BatchId>,
    pub title: Arc<str>,
    pub detail: Option<Arc<str>>,
}

/// How the adapter must answer a native permission request, right away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionVerdict {
    /// Pick the agent's own allow-once option (VM agent, or a human's grant).
    Allow { request: PermissionId },
    /// Pick the agent's own decline ("not now") option; the request went to
    /// the humans and the answer arrives later.
    Sent { request: PermissionId },
    /// No core to ask (replay tests): decline, nothing recorded.
    Decline,
}

/// How an adapter asks core about a native permission request. Cheap to clone.
#[derive(Clone)]
pub struct PermissionGate(
    Arc<dyn Fn(PermissionAsk) -> BoxFuture<'static, PermissionVerdict> + Send + Sync>,
);

impl PermissionGate {
    pub fn new(
        f: impl Fn(PermissionAsk) -> BoxFuture<'static, PermissionVerdict> + Send + Sync + 'static,
    ) -> Self {
        PermissionGate(Arc::new(f))
    }

    /// A gate that declines everything and records nothing (replay tests).
    pub fn decline_all() -> Self {
        PermissionGate::new(|_| Box::pin(async { PermissionVerdict::Decline }))
    }

    pub fn ask(&self, ask: PermissionAsk) -> BoxFuture<'static, PermissionVerdict> {
        (self.0)(ask)
    }
}

impl fmt::Debug for PermissionGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PermissionGate")
    }
}

/// The `_meta` an adapter attaches to a native decline after a request was
/// sent, so the agent (or a person reading the transcript) knows it was "not
/// now", not "no".
pub fn sent_meta(request: PermissionId) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "chaosControl".into(),
        json!({
            "status": "request_sent",
            "requestId": request.0,
            "message": sent_message(request),
        }),
    );
    m
}

fn sent_message(request: PermissionId) -> String {
    format!(
        "Not now: Chaos Control sent this to the humans as permission request #{request}. \
Carry on with something else or wait. The answer arrives as a message in a later turn \
(chat_permission_status shows it); once approved, retry the same call and it goes through once."
    )
}

// ---------- picking the agent's own option ----------

/// The kinds of option an agent offers, across protocol versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Other,
}

/// The agent's allow option: allow-once, else allow-always. Allow-always can
/// write a lasting rule into the agent's own settings (claude-agent-acp writes
/// local settings), so it is only picked when nothing narrower is offered.
pub fn pick_allow(options: &[(OptionKind, &str)]) -> Option<usize> {
    options
        .iter()
        .position(|(k, _)| *k == OptionKind::AllowOnce)
        .or_else(|| {
            options
                .iter()
                .position(|(k, _)| *k == OptionKind::AllowAlways)
        })
}

/// The agent's "not now" option: a reject-once that lets the turn continue,
/// else any reject-once, else reject-always. Codex offers two reject-once
/// options ("No, continue without running it" and "No, and tell Codex what to
/// do differently", which aborts the turn); the one that says "continue" wins.
pub fn pick_decline(options: &[(OptionKind, &str)]) -> Option<usize> {
    let once = |pred: &dyn Fn(&str) -> bool| {
        options
            .iter()
            .position(|(k, name)| *k == OptionKind::RejectOnce && pred(name))
    };
    once(&|name| name.to_lowercase().contains("continue"))
        .or_else(|| once(&|_| true))
        .or_else(|| {
            options
                .iter()
                .position(|(k, _)| *k == OptionKind::RejectAlways)
        })
}

// ---------- store ----------

const COLS: &str = "id, agent_id, channel_id, batch_id, source, title, detail, reason, state, \
asked_of, created_at, answered_by, answered_at, answer_note, used_at";

fn row(r: &Row<'_>) -> rusqlite::Result<PermissionRequest> {
    let asked: String = r.get(9)?;
    Ok(PermissionRequest {
        id: PermissionId(r.get(0)?),
        agent: MemberId(r.get(1)?),
        channel: ChannelId(r.get(2)?),
        batch: r.get::<_, Option<i64>>(3)?.map(BatchId),
        source: PermissionSource::parse(&r.get::<_, String>(4)?),
        title: Arc::from(r.get::<_, String>(5)?),
        detail: r.get::<_, Option<String>>(6)?.map(Arc::from),
        reason: r.get::<_, Option<String>>(7)?.map(Arc::from),
        state: PermissionState::parse(&r.get::<_, String>(8)?),
        asked_of: asked
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .map(MemberId)
            .collect(),
        created_at: r.get::<_, i64>(10)? as Millis,
        answered_by: r.get::<_, Option<i64>>(11)?.map(MemberId),
        answered_at: r.get::<_, Option<i64>>(12)?.map(|t| t as Millis),
        answer_note: r.get::<_, Option<String>>(13)?.map(Arc::from),
        used_at: r.get::<_, Option<i64>>(14)?.map(|t| t as Millis),
    })
}

/// A request about to be stored.
pub struct NewRequest<'a> {
    pub agent: MemberId,
    pub channel: ChannelId,
    pub batch: Option<BatchId>,
    pub source: PermissionSource,
    pub title: &'a str,
    pub detail: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub state: PermissionState,
    pub asked_of: &'a [MemberId],
    pub now: Millis,
}

pub fn insert(c: &Connection, r: &NewRequest<'_>) -> rusqlite::Result<PermissionRequest> {
    let asked = r
        .asked_of
        .iter()
        .map(|m| m.0.to_string())
        .collect::<Vec<_>>()
        .join(",");
    c.execute(
        "INSERT INTO permission_requests
            (agent_id, channel_id, batch_id, source, title, detail, reason, state, asked_of, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            r.agent.0,
            r.channel.0,
            r.batch.map(|b| b.0),
            r.source.as_str(),
            r.title,
            r.detail,
            r.reason,
            r.state.as_str(),
            asked,
            r.now as i64
        ],
    )?;
    let id = PermissionId(c.last_insert_rowid());
    get(c, id).map(|r| r.expect("just inserted"))
}

pub fn get(c: &Connection, id: PermissionId) -> rusqlite::Result<Option<PermissionRequest>> {
    c.query_row(
        &format!("SELECT {COLS} FROM permission_requests WHERE id = ?1"),
        [id.0],
        row,
    )
    .optional()
}

/// An approved request for exactly this (agent, channel, title) that has not
/// been used yet: the one-shot grant a retried tool call consumes.
pub fn unused_grant(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    title: &str,
) -> rusqlite::Result<Option<PermissionId>> {
    c.query_row(
        "SELECT id FROM permission_requests
         WHERE agent_id = ?1 AND channel_id = ?2 AND title = ?3
           AND state = 'approved' AND used_at IS NULL
         ORDER BY id LIMIT 1",
        params![agent.0, channel.0, title],
        |r| r.get(0).map(PermissionId),
    )
    .optional()
}

/// A still-pending request for the same thing, so asking twice never sends
/// the humans two requests.
pub fn pending_for(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    title: &str,
) -> rusqlite::Result<Option<PermissionRequest>> {
    c.query_row(
        &format!(
            "SELECT {COLS} FROM permission_requests
             WHERE agent_id = ?1 AND channel_id = ?2 AND title = ?3 AND state = 'pending'
             ORDER BY id LIMIT 1"
        ),
        params![agent.0, channel.0, title],
        row,
    )
    .optional()
}

/// The agent's requests in a channel, newest first.
pub fn of_agent(
    c: &Connection,
    agent: MemberId,
    channel: ChannelId,
    limit: usize,
) -> rusqlite::Result<Vec<PermissionRequest>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {COLS} FROM permission_requests
         WHERE agent_id = ?1 AND channel_id = ?2 ORDER BY id DESC LIMIT ?3"
    ))?;
    stmt.query_map(params![agent.0, channel.0, limit as i64], row)?
        .collect()
}

/// Every pending request in a channel, oldest first.
pub fn pending_in(c: &Connection, channel: ChannelId) -> rusqlite::Result<Vec<PermissionRequest>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {COLS} FROM permission_requests
         WHERE channel_id = ?1 AND state = 'pending' ORDER BY id"
    ))?;
    stmt.query_map([channel.0], row)?.collect()
}

/// Records a human's answer. Only a pending request can be answered; returns
/// whether this call answered it.
pub fn answer(
    c: &Connection,
    id: PermissionId,
    state: PermissionState,
    by: MemberId,
    note: Option<&str>,
    now: Millis,
) -> rusqlite::Result<bool> {
    let n = c.execute(
        "UPDATE permission_requests
         SET state = ?2, answered_by = ?3, answered_at = ?4, answer_note = ?5
         WHERE id = ?1 AND state = 'pending'",
        params![id.0, state.as_str(), by.0, now as i64, note],
    )?;
    Ok(n == 1)
}

pub fn mark_used(c: &Connection, id: PermissionId, now: Millis) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE permission_requests SET used_at = ?2 WHERE id = ?1 AND used_at IS NULL",
        params![id.0, now as i64],
    )?;
    Ok(())
}

// ---------- engine ----------

/// Why a human's answer was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnswerError {
    UnknownRequest,
    /// Already approved or denied (by someone else, or twice).
    AlreadyAnswered(PermissionState),
    /// Only humans in the request's channel can answer.
    NotAllowed,
    Store(String),
}

impl fmt::Display for AnswerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnswerError::UnknownRequest => f.write_str("no such permission request"),
            AnswerError::AlreadyAnswered(s) => write!(f, "already {}", s.as_str()),
            AnswerError::NotAllowed => {
                f.write_str("only a human in that channel can answer a permission request")
            }
            AnswerError::Store(e) => write!(f, "store error: {e}"),
        }
    }
}

impl From<rusqlite::Error> for AnswerError {
    fn from(e: rusqlite::Error) -> Self {
        AnswerError::Store(e.to_string())
    }
}

impl From<PostError> for AnswerError {
    fn from(e: PostError) -> Self {
        AnswerError::Store(e.to_string())
    }
}

fn handle_of(c: &Connection, id: MemberId) -> Arc<str> {
    store::member(c, id)
        .ok()
        .flatten()
        .map(|m| m.handle)
        .unwrap_or_else(|| Arc::from("someone"))
}

fn short(title: &str) -> String {
    let one_line = title.lines().next().unwrap_or_default();
    if one_line.chars().count() > 80 || title.contains('\n') {
        one_line.chars().take(80).collect::<String>() + "…"
    } else {
        one_line.to_string()
    }
}

/// The presence line while a request is pending (never "blocked": the agent
/// may well be doing something else).
pub fn pending_presence(c: &Connection, agent: MemberId, channel: ChannelId) -> Option<Arc<str>> {
    let pending: Vec<PermissionRequest> = of_agent(c, agent, channel, 20)
        .ok()?
        .into_iter()
        .filter(|r| r.state == PermissionState::Pending)
        .collect();
    let newest = pending.first()?;
    let more = match pending.len() {
        1 => String::new(),
        n => format!(" (+{} more)", n - 1),
    };
    Some(Arc::from(format!(
        "request pending: {} (#{}){more}",
        short(&newest.title),
        newest.id
    )))
}

impl Engine {
    /// Marks where an agent runs. Agents default to the host.
    pub fn set_runs_in(&mut self, agent: MemberId, runs_in: RunsIn) {
        self.runs_in.insert(agent, runs_in);
    }

    pub fn runs_in(&self, agent: MemberId) -> RunsIn {
        self.runs_in.get(&agent).copied().unwrap_or_default()
    }

    /// The humans a request from `agent` goes to: whoever woke the turn it was
    /// asked in, else every human in the channel.
    fn route(&self, channel: ChannelId, batch: Option<BatchId>) -> Vec<MemberId> {
        let c = self.conn();
        let mut out = Vec::new();
        if let Some(b) = batch {
            for t in store::batch_triggers(c, b).unwrap_or_default() {
                if let Ok(Some(m)) = store::message(c, t.id.message)
                    && let Some(a) = m.author
                    && let Ok(Some(who)) = store::member(c, a)
                    && who.kind == MemberKind::Human
                    && !out.contains(&a)
                {
                    out.push(a);
                }
            }
        }
        if out.is_empty() {
            out = store::members_of(c, channel)
                .unwrap_or_default()
                .into_iter()
                .filter(|m| m.kind == MemberKind::Human)
                .map(|m| m.id)
                .collect();
        }
        out
    }

    /// A native permission request from `agent`'s session in `channel`.
    /// Decides how the adapter answers right now (see the module docs).
    pub fn native_permission(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        ask: PermissionAsk,
    ) -> PermissionVerdict {
        match self.ask_permission(
            now,
            agent,
            channel,
            ask.turn,
            PermissionSource::Native,
            &ask.title,
            ask.detail.as_deref(),
            None,
        ) {
            Ok(r) => match r.state {
                PermissionState::Approved | PermissionState::AutoApproved => {
                    PermissionVerdict::Allow { request: r.id }
                }
                PermissionState::Pending | PermissionState::Denied => {
                    PermissionVerdict::Sent { request: r.id }
                }
            },
            Err(e) => {
                self.rt_note(agent, channel, format!("internal error: {e}"));
                PermissionVerdict::Decline
            }
        }
    }

    /// Records a request and works out its state: auto-approved in a VM, the
    /// grant a human already gave (used up here), the same request already
    /// pending, or a new pending request for the humans.
    #[allow(clippy::too_many_arguments)]
    pub fn ask_permission(
        &mut self,
        now: Millis,
        agent: MemberId,
        channel: ChannelId,
        turn: Option<BatchId>,
        source: PermissionSource,
        title: &str,
        detail: Option<&str>,
        reason: Option<&str>,
    ) -> Result<PermissionRequest, PostError> {
        let title = title.trim();
        let title = if title.is_empty() { "a tool" } else { title };
        let turn = match turn {
            Some(t) => Some(t),
            None => store::in_flight_batch(self.conn(), agent, channel)?.map(|b| b.id),
        };
        let handle = handle_of(self.conn(), agent);
        if self.runs_in(agent) == RunsIn::Vm {
            let r = self.store.tx(|c| {
                insert(
                    c,
                    &NewRequest {
                        agent,
                        channel,
                        batch: turn,
                        source,
                        title,
                        detail,
                        reason,
                        state: PermissionState::AutoApproved,
                        asked_of: &[],
                        now,
                    },
                )
            })?;
            self.rt_note(
                agent,
                channel,
                format!("ran {} (own VM, auto-approved)", short(title)),
            );
            self.effects
                .push(Effect::Event(CoreEvent::Permission(r.clone())));
            return Ok(r);
        }
        if let Some(grant) = unused_grant(self.conn(), agent, channel, title)? {
            if source == PermissionSource::ChatTool {
                // Asking ahead for something already approved: still approved,
                // and still unused until the tool call itself runs.
                return Ok(get(self.conn(), grant)?.expect("grant exists"));
            }
            self.store.tx(|c| mark_used(c, grant, now))?;
            let r = get(self.conn(), grant)?.expect("grant exists");
            self.rt_note(
                agent,
                channel,
                format!("running {} (approved, #{grant})", short(title)),
            );
            self.effects
                .push(Effect::Event(CoreEvent::Permission(r.clone())));
            return Ok(r);
        }
        if let Some(r) = pending_for(self.conn(), agent, channel, title)? {
            return Ok(r);
        }
        let asked_of = self.route(channel, turn);
        let r = self.store.tx(|c| {
            insert(
                c,
                &NewRequest {
                    agent,
                    channel,
                    batch: turn,
                    source,
                    title,
                    detail,
                    reason,
                    state: PermissionState::Pending,
                    asked_of: &asked_of,
                    now,
                },
            )
        })?;
        if self.policy().note_permission_requests {
            let who: Vec<String> = asked_of
                .iter()
                .map(|m| format!("@{}", handle_of(self.conn(), *m)))
                .collect();
            let why = reason
                .map(|w| format!(" ({})", w.trim()))
                .unwrap_or_default();
            let to = if who.is_empty() {
                String::new()
            } else {
                format!(", sent to {}", who.join(", "))
            };
            self.system_note(
                now,
                channel,
                &format!(
                    "@{handle} asked to run {}{why}: permission request #{}{to}. Any human here can approve or deny it; @{handle} carries on meanwhile.",
                    short(title),
                    r.id
                ),
            )?;
        }
        self.effects
            .push(Effect::Event(CoreEvent::Permission(r.clone())));
        self.publish_status(agent, channel);
        Ok(r)
    }

    /// A human approves or denies a pending request. The answer is posted as
    /// a system message and delivered to the agent as a wake (its next turn,
    /// right away if it is idle). An approval is a one-shot grant for the same
    /// tool call in this channel.
    pub fn answer_permission(
        &mut self,
        now: Millis,
        id: PermissionId,
        by: MemberId,
        approve: bool,
        note: Option<&str>,
    ) -> Result<PermissionRequest, AnswerError> {
        let req = get(self.conn(), id)?.ok_or(AnswerError::UnknownRequest)?;
        let who = store::member(self.conn(), by)?.ok_or(AnswerError::NotAllowed)?;
        if who.kind != MemberKind::Human || !store::is_member(self.conn(), req.channel, by)? {
            return Err(AnswerError::NotAllowed);
        }
        let state = if approve {
            PermissionState::Approved
        } else {
            PermissionState::Denied
        };
        let note = note.map(str::trim).filter(|n| !n.is_empty());
        if !self.store.tx(|c| answer(c, id, state, by, note, now))? {
            let now_state = get(self.conn(), id)?.map(|r| r.state).unwrap_or(req.state);
            return Err(AnswerError::AlreadyAnswered(now_state));
        }
        let req = get(self.conn(), id)?.expect("answered");
        let (agent, channel) = (req.agent, req.channel);
        let handle = handle_of(self.conn(), agent);
        let said = note.map(|n| format!(" \"{n}\"")).unwrap_or_default();
        let body = if approve {
            format!(
                "@{} approved @{handle}'s permission request #{id} to run {}{said}. @{handle}: retry it when you're ready; it goes through once.",
                who.handle,
                short(&req.title)
            )
        } else {
            format!(
                "@{} denied @{handle}'s permission request #{id} to run {}{said}. That's an answer, not a failure: carry on without it, or talk it through here.",
                who.handle,
                short(&req.title)
            )
        };
        let m = self.system_note(now, channel, &body)?;
        // Deliver the answer as a wake: one trigger on the note, no reply owed.
        let t = TriggerId {
            agent,
            channel,
            message: m.id,
        };
        self.store.tx::<_, PostError>(|c| {
            let batch = store::pending_batch_or_create(c, agent, channel, now)?;
            store::insert_trigger(c, t, WakeReason::Permission, 0, batch.id, now)?;
            Ok(())
        })?;
        // A human's answer is a fresh start, like a human ping.
        self.human_fresh_start(agent, channel);
        self.effects
            .push(Effect::Event(CoreEvent::Permission(req.clone())));
        self.pump(now, agent, channel);
        Ok(req)
    }

    /// Pending requests in `channel`, oldest first (for a client starting up).
    pub fn pending_permissions(
        &self,
        channel: ChannelId,
    ) -> rusqlite::Result<Vec<PermissionRequest>> {
        pending_in(self.conn(), channel)
    }
}

// ---------- chat tools ----------

pub const TOOL_NAMES: [&str; 2] = ["chat_request_permission", "chat_permission_status"];

pub fn tools(schema: fn(Value) -> Arc<serde_json::Map<String, Value>>) -> Vec<Tool> {
    vec![
        Tool::new(
            "chat_request_permission",
            "Ask the humans for permission to do something, without blocking. Returns at once with status \"request_sent\" and a request_id (or \"approved\" if you run in your own VM): keep working or wait, your call. The answer arrives as a message in a later turn; chat_permission_status shows it too. If the thing is a tool call that needs approval, use its exact title as `action`: once approved, that call goes through once.",
            schema(
                json!({"type": "object", "required": ["action"], "properties": {
                    "action": {"type": "string", "description": "What you want to do, e.g. the command or the tool call's title."},
                    "reason": {"type": "string", "description": "Why, in a sentence."}
                }}),
            ),
        ),
        Tool::new(
            "chat_permission_status",
            "Your permission requests in this channel and their answers (pending, approved, denied, auto_approved), newest first. Pass request_id for one.",
            schema(json!({"type": "object", "properties": {
                "request_id": {"type": "integer"}
            }})),
        ),
    ]
}

fn view(c: &Connection, r: &PermissionRequest) -> Value {
    json!({
        "request_id": r.id.0,
        "action": &*r.title,
        "reason": r.reason.as_deref(),
        "state": r.state.as_str(),
        "asked": r.asked_of.iter().map(|m| format!("@{}", handle_of(c, *m))).collect::<Vec<_>>(),
        "answered_by": r.answered_by.map(|m| format!("@{}", handle_of(c, m))),
        "note": r.answer_note.as_deref(),
        "used": r.used_at.is_some(),
    })
}

pub fn call_tool(
    engine: &mut Engine,
    now: Millis,
    scope: Scope,
    name: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    match name {
        "chat_request_permission" => {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .ok_or_else(|| ToolError("action is required".into()))?;
            let reason = args.get("reason").and_then(Value::as_str);
            let r = engine.ask_permission(
                now,
                scope.agent,
                scope.channel,
                None,
                PermissionSource::ChatTool,
                action,
                None,
                reason,
            )?;
            let mut v = view(engine.conn(), &r);
            let status = match r.state {
                PermissionState::AutoApproved => "approved",
                PermissionState::Approved => "approved",
                PermissionState::Denied => "denied",
                PermissionState::Pending => "request_sent",
            };
            v["status"] = json!(status);
            v["next"] = json!(match r.state {
                PermissionState::Pending =>
                    "Carry on with other work or wait; the answer arrives as a message in a later turn.",
                PermissionState::AutoApproved => "You run in your own VM: go ahead.",
                PermissionState::Approved => "Approved: go ahead.",
                PermissionState::Denied => "Denied: carry on without it, or talk it through.",
            });
            Ok(v)
        }
        "chat_permission_status" => {
            let c = engine.conn();
            if let Some(id) = args.get("request_id").and_then(Value::as_i64) {
                let r = get(c, PermissionId(id))?
                    .filter(|r| r.agent == scope.agent && r.channel == scope.channel)
                    .ok_or_else(|| ToolError("not_found: no such request of yours here".into()))?;
                return Ok(view(c, &r));
            }
            let all = of_agent(c, scope.agent, scope.channel, 20)?;
            Ok(json!({"requests": all.iter().map(|r| view(c, r)).collect::<Vec<_>>()}))
        }
        other => Err(ToolError(format!("unknown tool {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_prefers_once_over_always() {
        let opts = [
            (OptionKind::AllowAlways, "Always"),
            (OptionKind::AllowOnce, "Once"),
            (OptionKind::RejectOnce, "No"),
        ];
        assert_eq!(pick_allow(&opts), Some(1));
        assert_eq!(pick_allow(&[(OptionKind::RejectOnce, "No")]), None);
    }

    #[test]
    fn decline_prefers_the_option_that_lets_the_turn_continue() {
        // codex-acp's exec approval: abort first, then denied.
        let codex = [
            (OptionKind::AllowOnce, "Yes"),
            (
                OptionKind::RejectOnce,
                "No, and tell Codex what to do differently",
            ),
            (OptionKind::RejectOnce, "No, continue without running it"),
        ];
        assert_eq!(pick_decline(&codex), Some(2));
        let claude = [
            (OptionKind::AllowOnce, "Allow"),
            (OptionKind::RejectAlways, "Never"),
            (OptionKind::RejectOnce, "Reject"),
        ];
        assert_eq!(pick_decline(&claude), Some(2));
        assert_eq!(
            pick_decline(&[(OptionKind::RejectAlways, "Never")]),
            Some(0)
        );
        assert_eq!(pick_decline(&[(OptionKind::AllowOnce, "Yes")]), None);
    }

    #[test]
    fn runs_in_reads_from_config_words() {
        assert_eq!(
            serde_json::from_str::<RunsIn>("\"vm\"").unwrap(),
            RunsIn::Vm
        );
        assert_eq!(
            serde_json::from_str::<RunsIn>("\"host\"").unwrap(),
            RunsIn::Host
        );
        assert_eq!(RunsIn::default(), RunsIn::Host);
    }
}
