//! The ACP **v2 draft** client path (`acp_v2.rs`) end to end against the
//! scripted fake agent in v2 mode (`"protocol": 2`, see `chaos_core::fake`),
//! through the real engine, store, host and chat MCP server (see `common`),
//! plus the `fake-acp-agent` binary speaking raw v2 JSON-RPC over stdio.
//!
//! Before the fake had a v2 mode, the v2 client path was covered only by the
//! recorded claude-agent-acp transcripts in `replay.rs` (signed out, so no
//! agent output, tool calls or permission requests).

mod common;

use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use chaos_core::types::*;
use common::*;
use serde_json::{Value, json};

/// A v2-mode script with these turns.
fn v2(turns: Value) -> chaos_core::fake::Script {
    script(json!({"protocol": 2, "turns": turns}))
}

fn batches_in(state: &'static str, n: i64) -> impl Fn(&rusqlite::Connection) -> bool + Clone {
    move |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = ?1",
            [state],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == n
    }
}

async fn outcomes(h: &Harness) -> Vec<(String, Option<String>)> {
    h.core
        .with_engine(|e, _| {
            let mut st = e
                .conn()
                .prepare("SELECT state, outcome FROM batches ORDER BY id")
                .unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_agent_is_negotiated_and_replies_through_the_chat_tools() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            v2(json!([{"steps": [
                send_reply("on it: the flake is a timezone bug"),
                {"text": "I replied in the channel."}
            ]}])),
        )
        .await;
    let ping = h.say("@bot why is CI red?").await;
    let reply = h.wait_message("bot's reply", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(ping.id));
    assert_eq!(reply.origin, Origin::ChatTool);
    h.until("batch settled", batches_in("settled", 1)).await;
    h.quiesce().await;

    let adapter = h.agents["bot"].2.clone();
    assert_eq!(
        adapter.negotiated_version(),
        Some(2),
        "v2 offered first and selected: no v1 fallback"
    );
    assert_eq!(
        h.fake("bot").observed.lock().unwrap().protocols,
        vec![2],
        "one initialize, answered on v2"
    );
    let sessions = h.fake("bot").observed.lock().unwrap().sessions.clone();
    assert_eq!(sessions.len(), 1);
    let servers = sessions[0]["mcpServers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(
        servers[0]["type"],
        json!("http"),
        "the v2 capability was read"
    );
    assert_eq!(servers[0]["name"], json!("chaos-control"));

    assert_eq!(h.by(bot.id).await.len(), 1, "one reply, no fallback");
    assert_eq!(
        h.obligation(bot.id, ping.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(
        outcomes(&h).await,
        vec![("settled".to_string(), Some("Completed".to_string()))]
    );
}

/// Keyed upserts: chunks and an `agent_message` snapshot of the same message
/// are one message (the snapshot replaces, never appends); a tool call's
/// `completed` upsert arriving after the next message has started neither
/// clears that message nor counts as a new tool call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_fallback_text_follows_keyed_message_and_tool_call_upserts() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            v2(json!([{"steps": [
                {"text": "Looking at the logs."},
                {"tool_call": "Reading the CI logs"},
                {"gate": "look"},
                // Agent message 1 starts; the fake then upserts the tool call
                // to `completed`, after this text.
                {"text": "The flake is "},
                {"text": "a timezne bug."},
                // A snapshot of message 1 with the typo fixed.
                {"agent_message": "The flake is a timezone bug."},
                {"text": " Fix in #12."}
            ]}])),
        )
        .await;
    let ping = h.say("@bot why is CI red?").await;
    h.until("turn accepted", batches_in("accepted", 1)).await;
    let st = h.core.status(bot.id, h.main).await.unwrap();
    assert_eq!(st.glow, Glow::Active);
    assert_eq!(
        st.presence.as_deref(),
        Some("working: Reading the CI logs"),
        "a new v2 tool call is the agent's activity"
    );
    h.fake("bot").gates.open("look");

    let reply = h.wait_message("fallback", Some(bot.id)).await;
    assert_eq!(reply.origin, Origin::Fallback);
    assert_eq!(reply.reply_to, Some(ping.id));
    assert_eq!(
        &*reply.body, "The flake is a timezone bug. Fix in #12.",
        "the text since the tool call, with the snapshot replacing its chunks"
    );
    h.until("batch settled", batches_in("settled", 1)).await;
    assert_eq!(
        h.obligation(bot.id, ping.id).await,
        Some(ObligationState::Fallback)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_permission_request_is_declined_with_the_agents_own_option() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            v2(json!([{"steps": [
                {"permission": "rm -rf build/"},
                {"text": "I wasn't allowed to delete build/."}
            ]}])),
        )
        .await;
    let ping = h.say("@bot clean the build dir").await;
    let note = h.wait_message("system note", None).await;
    assert!(
        note.body.contains("@bot asked to run rm -rf build/"),
        "{}",
        note.body
    );
    assert!(
        note.body.contains("declined with its own \"Deny\" option"),
        "{}",
        note.body
    );
    let reply = h.wait_message("fallback", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(ping.id));
    assert_eq!(&*reply.body, "I wasn't allowed to delete build/.");
    let outcomes = h
        .fake("bot")
        .observed
        .lock()
        .unwrap()
        .permission_outcomes
        .clone();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["outcome"]["outcome"], json!("selected"));
    assert_eq!(
        outcomes[0]["outcome"]["optionId"],
        json!("deny"),
        "never the allow option: {}",
        outcomes[0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_interrupt_ends_the_turn_cancelled_and_unmet() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            v2(json!([
                {"steps": [
                    {"text": "Looking into it."},
                    {"tool_call": "Reading the CI logs"},
                    {"gate": "never"},
                    {"text": "Found it."}
                ]},
                {"steps": [{"text": "second: all green"}]}
            ])),
        )
        .await;
    let first = h.say("@bot why is CI red?").await;
    h.until("turn accepted", batches_in("accepted", 1)).await;
    h.core.interrupt(bot.id, h.main);
    h.until("turn settled", batches_in("settled", 1)).await;
    assert_eq!(h.fake("bot").observed.lock().unwrap().cancels, 1);
    assert_eq!(
        outcomes(&h).await,
        vec![("settled".to_string(), Some("Cancelled".to_string()))]
    );
    assert_eq!(
        h.obligation(bot.id, first.id).await,
        Some(ObligationState::Unmet)
    );
    assert!(h.by(bot.id).await.is_empty(), "no reply, no fallback");

    // The next turn on the same v2 session runs normally.
    let second = h.say("@bot and now?").await;
    let fallback = h.wait_message("second fallback", Some(bot.id)).await;
    assert_eq!(fallback.reply_to, Some(second.id));
    assert_eq!(&*fallback.body, "second: all green");
    assert_eq!(h.fake("bot").observed.lock().unwrap().sessions.len(), 1);
}

/// A scripted error after acceptance ends the v2 turn `idle` with an `error`
/// stop: a failed, accepted turn, never re-sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_error_stop_after_acceptance_is_not_retried() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            v2(json!([{"steps": [
                {"text": "Starting."},
                {"error": "model overloaded"}
            ]}])),
        )
        .await;
    let ping = h.say("@bot run it").await;
    let note = h.wait_message("system note", None).await;
    assert!(
        note.body
            .contains("@bot stopped before finishing (model overloaded)"),
        "{}",
        note.body
    );
    h.until("batch failed", batches_in("failed", 1)).await;
    assert_eq!(
        h.obligation(bot.id, ping.id).await,
        Some(ObligationState::Unmet)
    );
    for _ in 0..6 {
        h.advance(60_000).await;
    }
    assert_eq!(h.prompts("bot").len(), 1, "accepted, so never re-sent");
    assert_eq!(h.count("SELECT attempts FROM batches").await, 1);
    assert!(h.by(bot.id).await.is_empty());
}

/// The binary over real stdio, raw v2 JSON-RPC (the way the ACP TCK's v2
/// suite drives it), and the same binary still answering a v1 `initialize`.
#[test]
fn fake_agent_binary_speaks_v2_when_scripted() {
    let dir = std::env::temp_dir().join(format!("cc-fake-v2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join("script.json");
    std::fs::write(
        &script_path,
        json!({"protocol": 2, "turns": [
            {"steps": [{"text": "Working"}, {"tool_call": "Look"}, {"text": "Done"}]},
            {"steps": [{"tool_call": "Look again"}, {"sleep_ms": 30_000}]}
        ]})
        .to_string(),
    )
    .unwrap();
    let spawn = || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fake-acp-agent"))
            .env("FAKE_ACP_SCRIPT", &script_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        (child, stdin, rx)
    };
    let response = |id: u64| move |v: &Value| v["id"] == json!(id) && v.get("method").is_none();

    // v2.
    let (mut child, mut stdin, rx) = spawn();
    let mut send = |v: Value| {
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    };
    let frames = std::cell::RefCell::new(Vec::new());
    let next = |what: &str, pred: &dyn Fn(&Value) -> bool| -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok(v) => {
                    frames.borrow_mut().push(v.clone());
                    if pred(&v) {
                        return v;
                    }
                }
                Err(e) => panic!("no {what}: {e}; frames so far: {:#?}", frames.borrow()),
            }
        }
    };
    send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": 2,
            "clientInfo": {"name": "test", "version": "0"}}}));
    let init = next("initialize response", &response(0));
    assert_eq!(init["result"]["protocolVersion"], json!(2), "{init}");
    assert!(
        init["result"]["capabilities"]["session"]["mcp"]["http"].is_object(),
        "{init}"
    );
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "session/new",
        "params": {"cwd": dir.display().to_string()}}));
    let sid = next("session/new response", &response(1))["result"]["sessionId"].clone();
    assert!(sid.is_string(), "{sid}");
    let prompt = |id: u64, text: &str| {
        json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt",
            "params": {"sessionId": sid, "prompt": [{"type": "text", "text": text}]}})
    };
    let idle = |v: &Value| {
        v["method"] == "session/update"
            && v["params"]["update"]["sessionUpdate"] == "state_update"
            && v["params"]["update"]["state"] == "idle"
            && v["params"]["update"].get("stopReason").is_some()
    };

    send(prompt(2, "quick one"));
    let ack = next("prompt response", &response(2));
    assert!(ack["result"]["messageId"].is_string(), "{ack}");
    let end = next("idle with a stop reason", &idle);
    assert_eq!(end["params"]["update"]["stopReason"], json!("end_turn"));
    let seen = frames.borrow().clone();
    let updates: Vec<&Value> = seen
        .iter()
        .filter(|v| v["method"] == "session/update")
        .map(|v| &v["params"]["update"])
        .collect();
    let kinds: Vec<String> = updates
        .iter()
        .map(|u| match u["sessionUpdate"].as_str().unwrap() {
            "state_update" => format!("state:{}", u["state"].as_str().unwrap()),
            "tool_call_update" => format!("tool:{}", u["status"].as_str().unwrap()),
            other => other.to_string(),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "state:idle", // the fresh session is ready
            "state:running",
            "agent_message_chunk",
            "tool:in_progress",
            "agent_message_chunk",
            "tool:completed",
            "state:idle",
        ],
        "{updates:#?}"
    );
    let chunks: Vec<&Value> = updates
        .iter()
        .copied()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .collect();
    assert_ne!(
        chunks[0]["messageId"], chunks[1]["messageId"],
        "a tool call starts a new agent message"
    );
    let calls: Vec<&Value> = updates
        .iter()
        .copied()
        .filter(|u| u["sessionUpdate"] == "tool_call_update")
        .collect();
    assert_eq!(
        calls[0]["toolCallId"], calls[1]["toolCallId"],
        "completed is an upsert"
    );
    assert_eq!(calls[0]["title"], json!("Look"));
    assert!(calls[1].get("title").is_none(), "status-only upsert");

    // Cancel mid-turn: the turn ends idle/cancelled, the open call cancelled.
    send(prompt(3, "slow one"));
    next("second prompt response", &response(3));
    next("tool call", &|v| {
        v["params"]["update"]["sessionUpdate"] == "tool_call_update"
    });
    let started = std::time::Instant::now();
    send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}));
    let end = next("cancelled idle", &idle);
    assert_eq!(end["params"]["update"]["stopReason"], json!("cancelled"));
    assert!(started.elapsed() < Duration::from_secs(5));
    let _ = child.kill();
    let _ = child.wait();

    // The same script still answers a v1 client with v1.
    let (mut child, mut stdin, rx) = spawn();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let init = loop {
        let v = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if response(0)(&v) {
            break v;
        }
    };
    assert_eq!(init["result"]["protocolVersion"], json!(1), "{init}");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
