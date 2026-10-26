//! Cancelling a turn: the client side (`Core::interrupt` -> `AgentSession::interrupt`
//! -> ACP `session/cancel`) through the real engine, store, host and ACP adapter
//! against the scripted fake agent with a manual clock (see `common`), plus the
//! `fake-acp-agent` binary answering `cancelled` over real stdio (the ACP TCK's
//! ACP-CANCEL-001 finding, fixed in dfa8ea5).
//!
//! A slow turn is a `gate` the test never opens: the turn stays in flight until
//! the cancel ends it.

mod common;

use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use chaos_core::{engine::CoreEvent, types::*};
use common::*;
use serde_json::{Value, json};

fn cancels(h: &Harness, handle: &str) -> usize {
    h.fake(handle).observed.lock().unwrap().cancels
}

/// Waits (real time, bounded) until the fake has seen `n` cancels.
async fn wait_cancels(h: &Harness, handle: &str, n: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while cancels(h, handle) < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {n} session/cancel at {handle}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

/// A turn that streams text and a tool call, then works until cancelled.
fn slow_turn() -> Value {
    json!({"steps": [
        {"text": "Looking into it."},
        {"tool_call": "Reading the CI logs"},
        {"gate": "never"},
        {"text": "Found it."}
    ]})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_cancels_the_in_flight_turn_and_settles_it_unmet() {
    let mut h = Harness::new().await;
    let bot = h
        .agent("bot", script(json!({"turns": [slow_turn()]})))
        .await;
    let ping = h.say("@bot why is CI red?").await;
    h.until("turn accepted", batches_in("accepted", 1)).await;
    let st = h.core.status(bot.id, h.main).await.unwrap();
    assert_eq!(st.glow, Glow::Active);
    assert_eq!(st.presence.as_deref(), Some("working: Reading the CI logs"));

    h.core.interrupt(bot.id, h.main);
    wait_cancels(&h, "bot", 1).await;
    // The fake answers `stopReason: cancelled`; the turn settles, no hang.
    h.until("turn settled", batches_in("settled", 1)).await;
    let status = h
        .wait_for("presence after the cancel", |e| {
            matches!(e, CoreEvent::Agent(s) if s.agent == bot.id
                && s.presence.as_deref() == Some("turn ended without a reply"))
        })
        .await;
    let CoreEvent::Agent(status) = status else {
        unreachable!()
    };
    assert_eq!(status.glow, Glow::Active, "the session stays open");

    assert_eq!(
        outcomes(&h).await,
        vec![("settled".to_string(), Some("Cancelled".to_string()))]
    );
    // A cancelled turn never posts a fallback, even with final text streamed:
    // the obligation is `unmet` (docs/agents.md, "Reply obligations").
    assert_eq!(
        h.obligation(bot.id, ping.id).await,
        Some(ObligationState::Unmet)
    );
    assert!(h.by(bot.id).await.is_empty(), "no reply, no fallback");
    assert_eq!(
        h.count(
            "SELECT COUNT(*) FROM batches WHERE state IN ('pending', 'dispatched', 'accepted')"
        )
        .await,
        0,
        "nothing in flight or pending"
    );
    assert_eq!(
        h.count("SELECT COUNT(*) FROM messages WHERE origin = 'system'")
            .await,
        0,
        "a cancel is not a failure: no system note"
    );

    // Accepted, so never re-sent: a minute of fake time brings no retry.
    h.advance(60_000).await;
    assert_eq!(h.prompts("bot").len(), 1);
    assert_eq!(h.count("SELECT attempts FROM batches").await, 1);
    assert_eq!(cancels(&h, "bot"), 1, "one session/cancel");
    let st = h.core.status(bot.id, h.main).await.unwrap();
    assert_eq!(
        (st.glow, st.presence.as_deref()),
        (Glow::Active, Some("turn ended without a reply"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_with_no_turn_running_is_harmless() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [send_reply("pong $N")]}]})),
        )
        .await;

    // No session yet: nothing to cancel, nothing sent, nothing woken.
    h.core.interrupt(bot.id, h.main);
    // Unknown agent x channel: ignored.
    h.core.interrupt(MemberId(9_999), h.main);
    h.core.interrupt(bot.id, ChannelId(9_999));
    h.quiesce().await;
    assert_eq!(cancels(&h, "bot"), 0);
    assert_eq!(h.count("SELECT COUNT(*) FROM batches").await, 0);
    assert!(h.prompts("bot").is_empty());
    assert_eq!(
        h.core.status(bot.id, h.main).await.unwrap().glow,
        Glow::Sleeping
    );

    let first = h.say("@bot ping").await;
    let reply = h.wait_message("first reply", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(first.id));
    h.until("first turn settled", batches_in("settled", 1))
        .await;

    // An open but idle session: the cancel goes out (ACP says an idle agent
    // ignores it) and changes nothing on our side.
    h.core.interrupt(bot.id, h.main);
    wait_cancels(&h, "bot", 1).await;
    h.quiesce().await;
    assert_eq!(
        outcomes(&h).await,
        vec![("settled".to_string(), Some("Completed".to_string()))]
    );
    assert_eq!(
        h.obligation(bot.id, first.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(h.history().await.len(), 2, "no new messages or notes");
    assert_eq!(h.prompts("bot").len(), 1);
    assert_eq!(
        h.core.status(bot.id, h.main).await.unwrap().glow,
        Glow::Active
    );

    // And the agent still answers the next ping normally.
    let second = h.say("@bot ping again").await;
    let reply = h.wait_message("second reply", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(second.id));
    assert_eq!(&*reply.body, "pong 1");
    h.until("second turn settled", batches_in("settled", 2))
        .await;
    assert_eq!(
        h.obligation(bot.id, second.id).await,
        Some(ObligationState::Satisfied)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_does_not_leak_into_the_next_turn() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [
                slow_turn(),
                {"steps": [{"tool_call": "Checking"}, send_reply("second: the cache key")]},
                {"steps": [{"text": "third: all green"}]}
            ]})),
        )
        .await;
    let first = h.say("@bot why is CI red?").await;
    h.until("first turn accepted", batches_in("accepted", 1))
        .await;
    // A second ping mid-turn waits in the pending batch.
    let second = h.say("@bot also, why is it slow?").await;
    h.until("second ping pending", batches_in("pending", 1))
        .await;

    h.core.interrupt(bot.id, h.main);
    wait_cancels(&h, "bot", 1).await;
    // The cancel ends only the in-flight turn; the pending batch is then
    // delivered as a fresh turn that runs to completion.
    let reply = h.wait_message("second turn's reply", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(second.id));
    assert_eq!(&*reply.body, "second: the cache key");
    h.until("both turns settled", batches_in("settled", 2))
        .await;
    assert_eq!(h.wait_prompts("bot", 2).await.len(), 2);
    let prompt = &h.prompts("bot")[1];
    assert!(prompt.contains(&format!("You owe a reply to id:{}", second.id)));
    assert!(
        !prompt.contains(&format!("You owe a reply to id:{}", first.id)),
        "the cancelled turn's trigger is not re-delivered"
    );
    assert_eq!(
        h.obligation(bot.id, first.id).await,
        Some(ObligationState::Unmet)
    );
    assert_eq!(
        h.obligation(bot.id, second.id).await,
        Some(ObligationState::Satisfied)
    );

    // Once idle, a later ping also completes, with a fallback reply: the
    // cancel left no state behind in the adapter, the session or the fake.
    let third = h.say("@bot and now?").await;
    let fallback = h.wait_message("third turn's fallback", Some(bot.id)).await;
    assert_eq!(fallback.origin, Origin::Fallback);
    assert_eq!(fallback.reply_to, Some(third.id));
    assert_eq!(&*fallback.body, "third: all green");
    h.until("three turns settled", batches_in("settled", 3))
        .await;
    assert_eq!(
        outcomes(&h).await,
        vec![
            ("settled".to_string(), Some("Cancelled".to_string())),
            ("settled".to_string(), Some("Completed".to_string())),
            ("settled".to_string(), Some("Completed".to_string())),
        ]
    );
    assert_eq!(
        h.obligation(bot.id, third.id).await,
        Some(ObligationState::Fallback)
    );
    assert_eq!(cancels(&h, "bot"), 1);
    assert_eq!(h.prompts("bot").len(), 3);
}

/// The `fake-acp-agent` binary over real stdio, raw ACP v1 JSON-RPC (the way
/// the ACP TCK drives it): `session/cancel` mid-turn ends the turn with
/// `stopReason: cancelled`, and the next prompt runs normally.
#[test]
fn fake_agent_binary_answers_cancelled_when_cancelled_mid_turn() {
    let dir = std::env::temp_dir().join(format!("cc-fake-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join("script.json");
    std::fs::write(
        &script_path,
        json!({"turns": [
            {"steps": [{"text": "Working"}, {"tool_call": "Look"}, {"sleep_ms": 30_000}, {"text": "Done"}]},
            {"steps": [{"text": "Second"}]}
        ]})
        .to_string(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fake-acp-agent"))
        .env("FAKE_ACP_SCRIPT", &script_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
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
    let mut send = |v: Value| {
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    };
    // Reads frames until `pred` matches; the 30 s turn must never be waited out.
    let next = |what: &str, pred: &dyn Fn(&Value) -> bool| -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok(v) if pred(&v) => return v,
                Ok(_) => {}
                Err(e) => panic!("no {what}: {e}"),
            }
        }
    };
    let response = |id: u64| move |v: &Value| v["id"] == json!(id) && v.get("method").is_none();

    send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}}));
    let init = next("initialize response", &response(0));
    assert_eq!(init["result"]["protocolVersion"], json!(1));
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "session/new",
        "params": {"cwd": dir.display().to_string(), "mcpServers": []}}));
    let sid = next("session/new response", &response(1))["result"]["sessionId"].clone();
    assert!(sid.is_string(), "{sid}");

    let prompt = |id: u64, text: &str| {
        json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt",
            "params": {"sessionId": sid, "prompt": [{"type": "text", "text": text}]}})
    };
    send(prompt(2, "slow one"));
    // Mid-turn: the tool call has streamed, the sleep is running.
    next("tool_call update", &|v| {
        v["method"] == "session/update" && v["params"]["update"]["sessionUpdate"] == "tool_call"
    });
    let started = std::time::Instant::now();
    send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}));
    let answer = next("cancelled prompt response", &response(2));
    assert_eq!(
        answer["result"]["stopReason"],
        json!("cancelled"),
        "{answer}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the cancel ended the turn"
    );

    // The cancel does not leak into the next prompt.
    send(prompt(3, "quick one"));
    let answer = next("second prompt response", &response(3));
    assert_eq!(
        answer["result"]["stopReason"],
        json!("end_turn"),
        "{answer}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
