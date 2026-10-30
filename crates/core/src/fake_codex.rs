//! A scripted, in-process Codex app-server for tests: it answers the
//! JSON-RPC the native Codex adapter (`crate::codex`) sends, over an
//! in-memory pipe, and records what it was sent.
//!
//! Each `turn/start` plays the next scripted turn: agent messages, tool
//! items, approval requests (the adapter's answer is recorded) and a final
//! status. A `turn/interrupt` during a `Sleep` ends the turn `interrupted`.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::adapter::{AgentError, Probe};
use crate::codex::{CodexPipe, CodexTransport};

#[derive(Clone, Debug)]
pub enum Step {
    /// An `agentMessage` item.
    Message(String),
    /// A `commandExecution` item (activity only).
    Command(String),
    /// `item/commandExecution/requestApproval` for this command; the
    /// adapter's decision is recorded in [`FakeCodex::decisions`].
    Approval(String),
    Sleep(u64),
}

#[derive(Clone, Debug, Default)]
pub struct Turn {
    pub steps: Vec<Step>,
    /// `completed` (default), `failed` or `interrupted`.
    pub status: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Script {
    /// `(id, displayName)` for `model/list`.
    pub models: Vec<(String, String)>,
    /// Answer `thread/start` with this error (e.g. a sign-in message).
    pub thread_error: Option<String>,
    pub turns: Vec<Turn>,
}

/// The transport: every `connect` starts a fresh fake server on the script.
#[derive(Clone)]
pub struct FakeCodex {
    script: Script,
    /// Every request and notification the adapter sent: `{method, params}`.
    pub received: Arc<Mutex<Vec<Value>>>,
    /// Decisions the adapter gave approval requests, in order.
    pub decisions: Arc<Mutex<Vec<String>>>,
}

impl FakeCodex {
    pub fn new(script: Script) -> Self {
        FakeCodex {
            script,
            received: Arc::default(),
            decisions: Arc::default(),
        }
    }

    pub fn methods(&self) -> Vec<String> {
        self.received
            .lock()
            .expect("lock")
            .iter()
            .filter_map(|v| v["method"].as_str().map(str::to_string))
            .collect()
    }

    /// The params of the first `method` received.
    pub fn request(&self, method: &str) -> Option<Value> {
        self.received
            .lock()
            .expect("lock")
            .iter()
            .find(|v| v["method"] == method)
            .map(|v| v["params"].clone())
    }
}

impl CodexTransport for FakeCodex {
    fn describe(&self) -> String {
        "fake codex app-server".into()
    }

    fn probe(&self) -> Probe {
        Probe::Plausible
    }

    fn connect(&self) -> Result<CodexPipe, AgentError> {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (cr, cw) = tokio::io::split(client);
        let (sr, sw) = tokio::io::split(server);
        let me = self.clone();
        tokio::spawn(async move { me.serve(sr, sw).await });
        Ok(CodexPipe {
            write: Box::new(cw),
            read: Box::new(cr),
            child: None,
        })
    }
}

type Writer = Arc<tokio::sync::Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>>;
type Answers = Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>>;

async fn send(w: &Writer, v: Value) {
    let mut w = w.lock().await;
    let _ = w.write_all(format!("{v}\n").as_bytes()).await;
    let _ = w.flush().await;
}

impl FakeCodex {
    async fn serve(
        self,
        read: tokio::io::ReadHalf<tokio::io::DuplexStream>,
        write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let w: Writer = Arc::new(tokio::sync::Mutex::new(write));
        let mut turns: VecDeque<Turn> = self.script.turns.clone().into();
        let interrupted = Arc::new(Mutex::new(false));
        let answers: Answers = Arc::default();
        let mut next_turn = 0;
        let mut next_req = 1000;
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let id = msg.get("id").cloned();
            let Some(method) = msg["method"].as_str().map(str::to_string) else {
                // An answer to one of our approval requests.
                if let Some(id) = id.and_then(|i| i.as_i64())
                    && let Some(tx) = answers.lock().expect("lock").remove(&id)
                {
                    let _ = tx.send(msg["result"].clone());
                }
                continue;
            };
            self.received
                .lock()
                .expect("lock")
                .push(json!({ "method": method, "params": msg["params"] }));
            let Some(id) = id else { continue }; // a notification
            match method.as_str() {
                "initialize" => {
                    send(
                        &w,
                        json!({"id": id, "result": {"userAgent": "codex_fake/0.0.1"}}),
                    )
                    .await
                }
                "model/list" => {
                    let data: Vec<_> = self
                        .script
                        .models
                        .iter()
                        .map(|(m, n)| json!({"id": m, "model": m, "displayName": n}))
                        .collect();
                    send(
                        &w,
                        json!({"id": id, "result": {"data": data, "nextCursor": null}}),
                    )
                    .await
                }
                "thread/start" => match &self.script.thread_error {
                    Some(e) => {
                        send(
                            &w,
                            json!({"id": id, "error": {"code": -32603, "message": e}}),
                        )
                        .await
                    }
                    None => {
                        let model = msg["params"]["model"].as_str().unwrap_or("fake-default");
                        send(
                            &w,
                            json!({"id": id, "result": {"thread": {"id": "thr_1"}, "model": model}}),
                        )
                        .await
                    }
                },
                "turn/interrupt" => {
                    *interrupted.lock().expect("lock") = true;
                    send(&w, json!({"id": id, "result": {}})).await;
                }
                "turn/start" => {
                    next_turn += 1;
                    let turn_id = format!("turn_{next_turn}");
                    send(
                        &w,
                        json!({"id": id, "result": {"turn": {"id": turn_id, "status": "inProgress"}}}),
                    )
                    .await;
                    let turn = turns.pop_front().unwrap_or_default();
                    *interrupted.lock().expect("lock") = false;
                    let ctx = Play {
                        w: w.clone(),
                        interrupted: interrupted.clone(),
                        answers: answers.clone(),
                        decisions: self.decisions.clone(),
                        req_id: next_req,
                    };
                    next_req += 100;
                    tokio::spawn(async move { ctx.play(turn, turn_id).await });
                }
                _ => {
                    send(
                        &w,
                        json!({"id": id, "error": {"code": -32601, "message": "unknown method"}}),
                    )
                    .await
                }
            }
        }
    }
}

struct Play {
    w: Writer,
    interrupted: Arc<Mutex<bool>>,
    answers: Answers,
    decisions: Arc<Mutex<Vec<String>>>,
    req_id: i64,
}

impl Play {
    fn interrupted(&self) -> bool {
        *self.interrupted.lock().expect("lock")
    }

    async fn note(&self, method: &str, params: Value) {
        send(&self.w, json!({"method": method, "params": params})).await
    }

    async fn play(mut self, turn: Turn, turn_id: String) {
        self.note(
            "turn/started",
            json!({"threadId": "thr_1", "turn": {"id": turn_id}}),
        )
        .await;
        for (i, step) in turn.steps.iter().enumerate() {
            if self.interrupted() {
                break;
            }
            let item_id = format!("item_{i}");
            match step {
                Step::Message(text) => {
                    self.note(
                        "item/completed",
                        json!({"threadId": "thr_1", "turnId": turn_id,
                               "item": {"type": "agentMessage", "id": item_id, "text": text}}),
                    )
                    .await
                }
                Step::Command(cmd) => {
                    self.note(
                        "item/started",
                        json!({"threadId": "thr_1", "turnId": turn_id,
                               "item": {"type": "commandExecution", "id": item_id, "command": cmd}}),
                    )
                    .await
                }
                Step::Approval(cmd) => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    self.answers.lock().expect("lock").insert(self.req_id, tx);
                    send(
                        &self.w,
                        json!({"id": self.req_id, "method": "item/commandExecution/requestApproval",
                               "params": {"threadId": "thr_1", "turnId": turn_id,
                                          "itemId": item_id, "command": cmd}}),
                    )
                    .await;
                    self.req_id += 1;
                    if let Ok(answer) = rx.await {
                        self.decisions
                            .lock()
                            .expect("lock")
                            .push(answer["decision"].as_str().unwrap_or("?").to_string());
                    }
                }
                Step::Sleep(ms) => {
                    let until = tokio::time::Instant::now() + Duration::from_millis(*ms);
                    while tokio::time::Instant::now() < until && !self.interrupted() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            }
        }
        let status = if self.interrupted() {
            "interrupted".to_string()
        } else {
            turn.status.clone().unwrap_or_else(|| "completed".into())
        };
        let mut t = json!({"id": turn_id, "status": status});
        if let Some(e) = &turn.error {
            t["error"] = json!({"message": e});
        }
        self.note("turn/completed", json!({"threadId": "thr_1", "turn": t}))
            .await;
    }
}
