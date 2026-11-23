//! STRM-612 slice (4): real-orchestrator approval replay/E2E tests.
//!
//! These tests exercise approval flows through the real Core/engine/host with
//! fake upstream agents (ACP fake agent over in-memory transport, OpenCode V2
//! adapter over a local axum server). They assert no silent approval, no
//! double execution/reply, and fail-closed behavior on disconnect/loss.

mod common;

use chaos_core::{approval::ApprovalOutcomeState, engine::CoreEvent};
use common::*;
use serde_json::json;

fn batches_in(state: &'static str, n: i64) -> impl Fn(&rusqlite::Connection) -> bool + Clone {
    move |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = ?1",
            [state],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            == n
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_permission_allow_is_resolved_through_orchestrator() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                {"permission": "rm -rf build/"},
                {"mcp": {"tool": "chat_send", "args": {"body": "Deleted build.", "reply_to": "$TRIGGER", "client_request_id": "req-$N"}}}
            ]}]})),
        )
        .await;
    let ping = h.say("@bot clean the build dir").await;
    let asked = h
        .wait_for(
            "approval asked",
            |e| matches!(e, CoreEvent::ApprovalRequested(a) if a.agent == bot.id),
        )
        .await;
    let req = match asked {
        CoreEvent::ApprovalRequested(req) => *req,
        _ => unreachable!(),
    };
    // Pick the allow option.
    let allow = match &req.choices {
        chaos_core::approval::ApprovalChoices::ProviderOptions { options } => options
            .iter()
            .find(|o| o.kind.as_deref() == Some("allow_once") || o.id.as_ref() == "allow")
            .expect("allow option")
            .id
            .clone(),
        other => panic!("expected provider options, got {other:?}"),
    };
    h.core.resolve_approval(&req.cc_request_id, &allow).unwrap();

    let reply = h.wait_message("bot reply", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(ping.id));
    h.until("batch settled", batches_in("settled", 1)).await;
    h.quiesce().await;

    let outcomes = h
        .fake("bot")
        .observed
        .lock()
        .unwrap()
        .permission_outcomes
        .clone();
    assert_eq!(
        outcomes.len(),
        1,
        "exactly one permission outcome: {outcomes:?}"
    );
    assert_eq!(outcomes[0]["outcome"]["outcome"], json!("selected"));
    assert_eq!(outcomes[0]["outcome"]["optionId"], json!(allow.as_ref()));

    let recorded = h
        .core
        .approval_outcome(&req.cc_request_id)
        .unwrap()
        .expect("outcome recorded");
    assert_eq!(recorded.state, ApprovalOutcomeState::Allowed);

    assert_eq!(h.by(bot.id).await.len(), 1, "no double reply");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_permission_deny_is_resolved_through_orchestrator() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                {"permission": "rm -rf build/"},
                {"text": "I will not delete it."}
            ]}]})),
        )
        .await;
    let ping = h.say("@bot clean the build dir").await;
    let asked = h
        .wait_for(
            "approval asked",
            |e| matches!(e, CoreEvent::ApprovalRequested(a) if a.agent == bot.id),
        )
        .await;
    let req = match asked {
        CoreEvent::ApprovalRequested(req) => *req,
        _ => unreachable!(),
    };
    let deny = match &req.choices {
        chaos_core::approval::ApprovalChoices::ProviderOptions { options } => options
            .iter()
            .find(|o| o.kind.as_deref() == Some("reject_once") || o.id.as_ref() == "deny")
            .expect("deny option")
            .id
            .clone(),
        other => panic!("expected provider options, got {other:?}"),
    };
    h.core.resolve_approval(&req.cc_request_id, &deny).unwrap();

    h.until("batch settled", batches_in("settled", 1)).await;
    h.quiesce().await;

    let outcomes = h
        .fake("bot")
        .observed
        .lock()
        .unwrap()
        .permission_outcomes
        .clone();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["outcome"]["outcome"], json!("selected"));
    assert_eq!(outcomes[0]["outcome"]["optionId"], json!(deny.as_ref()));

    let recorded = h
        .core
        .approval_outcome(&req.cc_request_id)
        .unwrap()
        .expect("outcome recorded");
    assert_eq!(recorded.state, ApprovalOutcomeState::Denied);
    assert_eq!(
        h.obligation(bot.id, ping.id).await,
        Some(chaos_core::types::ObligationState::Ignored)
    );
    assert!(h.by(bot.id).await.is_empty(), "final text stays private");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_permission_replay_after_resolution_is_rejected() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                {"permission": "rm -rf build/"},
                {"text": "done"}
            ]}]})),
        )
        .await;
    let _ping = h.say("@bot clean the build dir").await;
    let asked = h
        .wait_for(
            "approval asked",
            |e| matches!(e, CoreEvent::ApprovalRequested(a) if a.agent == bot.id),
        )
        .await;
    let req = match asked {
        CoreEvent::ApprovalRequested(req) => *req,
        _ => unreachable!(),
    };
    let deny = match &req.choices {
        chaos_core::approval::ApprovalChoices::ProviderOptions { options } => options
            .iter()
            .find(|o| o.kind.as_deref() == Some("reject_once") || o.id.as_ref() == "deny")
            .expect("deny option")
            .id
            .clone(),
        other => panic!("expected provider options, got {other:?}"),
    };
    h.core.resolve_approval(&req.cc_request_id, &deny).unwrap();
    h.until("batch settled", batches_in("settled", 1)).await;
    h.quiesce().await;

    // Replaying the same resolution must not produce a second provider effect.
    let err = h
        .core
        .resolve_approval(&req.cc_request_id, &deny)
        .unwrap_err();
    assert!(
        err.contains("resolved")
            || err.contains("unknown")
            || err.contains("not pending")
            || err.contains("no longer pending"),
        "unexpected replay error: {err}"
    );
    h.quiesce().await;
    let outcomes = h
        .fake("bot")
        .observed
        .lock()
        .unwrap()
        .permission_outcomes
        .clone();
    assert_eq!(outcomes.len(), 1, "no double execution: {outcomes:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_permission_cancel_fails_closed() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                {"permission": "rm -rf build/"},
                {"text": "cancelled"}
            ]}]})),
        )
        .await;
    let _ping = h.say("@bot clean the build dir").await;
    let asked = h
        .wait_for(
            "approval asked",
            |e| matches!(e, CoreEvent::ApprovalRequested(a) if a.agent == bot.id),
        )
        .await;
    let req = match asked {
        CoreEvent::ApprovalRequested(req) => *req,
        _ => unreachable!(),
    };
    h.core.cancel_approval(&req.cc_request_id).unwrap();
    h.until("batch settled", batches_in("settled", 1)).await;
    h.quiesce().await;

    let outcomes = h
        .fake("bot")
        .observed
        .lock()
        .unwrap()
        .permission_outcomes
        .clone();
    assert_eq!(
        outcomes.len(),
        1,
        "provider got one fail-closed answer: {outcomes:?}"
    );
    // Cancelled/not-resolved should not be reported as allowed.
    let recorded = h
        .core
        .approval_outcome(&req.cc_request_id)
        .unwrap()
        .expect("outcome recorded");
    assert_ne!(recorded.state, ApprovalOutcomeState::Allowed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_permission_interrupt_cancels_pending_and_fails_closed() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                {"permission": "rm -rf build/"},
                {"text": "after interrupt"}
            ]}]})),
        )
        .await;
    let ping = h.say("@bot clean the build dir").await;
    let asked = h
        .wait_for(
            "approval asked",
            |e| matches!(e, CoreEvent::ApprovalRequested(a) if a.agent == bot.id),
        )
        .await;
    let req = match asked {
        CoreEvent::ApprovalRequested(req) => *req,
        _ => unreachable!(),
    };

    h.core.interrupt(bot.id, h.main);
    // Interrupt/cancel may settle as settled, failed, or leave the turn ended;
    // the key invariant is fail-closed with no double execution.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let done = h
            .core
            .with_engine(|e, _| {
                for proj in e.store().projects().unwrap_or_default() {
                    if let Ok(ag) = e.store().project_agents(&proj.id)
                        && let Ok(state) = ag.query_row(
                            "SELECT state FROM batches ORDER BY id DESC LIMIT 1",
                            [],
                            |r| r.get::<_, String>(0),
                        )
                    {
                        return matches!(state.as_str(), "settled" | "failed");
                    }
                }
                false
            })
            .await
            .unwrap();
        if done || tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    h.quiesce().await;

    let recorded = h.core.approval_outcome(&req.cc_request_id).unwrap();
    if let Some(outcome) = recorded {
        assert_ne!(
            outcome.state,
            ApprovalOutcomeState::Allowed,
            "interrupt must not auto-allow"
        );
    }
    let outcomes = h
        .fake("bot")
        .observed
        .lock()
        .unwrap()
        .permission_outcomes
        .clone();
    assert!(
        outcomes.len() <= 1,
        "no duplicate permission execution: {outcomes:?}"
    );
    let replies = h.by(bot.id).await;
    assert!(
        replies.is_empty(),
        "no successful reply after interrupted approval: {replies:?}"
    );
    let _ = ping;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_allow_through_real_orchestrator() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::{StreamExt, stream};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route(
            "/api/session",
            post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }),
        )
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![
                        Ok::<_, std::convert::Infallible>(Event::default().data(
                            r#"{"type":"session.tool.input.ended","data":{"sessionID":"s1","id":"tool-1","input":{"command":"echo SAFE"}}}"#,
                        )),
                        Ok(Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"shell","resources":["echo SAFE"],"save":["echo *"],"source":{"type":"tool","messageID":"m1","id":"tool-1"}}}"#,
                        )),
                    ])
                    .chain(stream::pending());
                    Sse::new(events).keep_alive(KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                async move {
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();

    let _ping = core.post(hx.id, main, "@v2 run it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut seen = Vec::new();
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(e)) => seen.push(format!("{e:?}")),
            Ok(None) => panic!("core stopped; seen={seen:#?}"),
            Err(_) => panic!("no approval request; seen={seen:#?}"),
        }
    };
    assert_eq!(req.cc_request_id.as_str(), "opencode-v2:s1:p1");

    core.resolve_approval(&req.cc_request_id, "once").unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
        .await
        .expect("provider reply arrives")
        .expect("reply channel open");
    assert_eq!(reply["decision"], "once");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let outcome = loop {
        if let Some(outcome) = core.approval_outcome(&req.cc_request_id).unwrap() {
            break outcome;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("outcome not recorded");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(outcome.state, ApprovalOutcomeState::Allowed);

    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_deny_through_real_orchestrator() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::{StreamExt, stream};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
        )
        .route(
            "/api/session/s1/prompt",
            post({
                let prompted = prompted.clone();
                move || { let prompted = prompted.clone(); async move { prompted.notify_one(); Json(serde_json::json!({ "data": {} })) } }
            }),
        )
        .route(
            "/api/event",
            get(move || {
                let prompted = sse_ready.clone();
                async move {
                    prompted.notified().await;
                    let events = stream::iter(vec![
                        Ok::<_, std::convert::Infallible>(Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"shell","resources":["echo SAFE"]}}"#,
                        )),
                    ])
                    .chain(stream::pending());
                    Sse::new(events).keep_alive(KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                async move { let _ = reply_tx.send(body); axum::http::StatusCode::NO_CONTENT }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 run it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };
    core.resolve_approval(&req.cc_request_id, "reject").unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
        .await
        .expect("provider reply arrives")
        .expect("reply channel open");
    assert_eq!(reply["decision"], "reject");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let outcome = loop {
        if let Some(outcome) = core.approval_outcome(&req.cc_request_id).unwrap() {
            break outcome;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("outcome not recorded");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(outcome.state, ApprovalOutcomeState::Denied);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_replay_after_resolution_is_rejected() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::{StreamExt, stream};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::mpsc;

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let reply_count = Arc::new(AtomicUsize::new(0));
    let reply_count_route = reply_count.clone();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"shell","resources":["echo SAFE"]}}"#,
                        ),
                    )])
                    .chain(stream::pending());
                    Sse::new(events).keep_alive(KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                let reply_count = reply_count_route.clone();
                async move {
                    reply_count.fetch_add(1, Ordering::SeqCst);
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 run it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };

    core.resolve_approval(&req.cc_request_id, "reject").unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
        .await
        .expect("provider reply arrives")
        .expect("reply channel open");
    assert_eq!(reply["decision"], "reject");

    let err = core
        .resolve_approval(&req.cc_request_id, "reject")
        .unwrap_err();
    assert!(
        err.contains("resolved")
            || err.contains("unknown")
            || err.contains("not pending")
            || err.contains("no longer pending"),
        "unexpected replay error: {err}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        reply_count.load(Ordering::SeqCst),
        1,
        "replay must not send a second provider reply"
    );
    let recorded = core.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(recorded.state, ApprovalOutcomeState::Denied);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_interrupt_fails_closed_through_real_orchestrator() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::{StreamExt, stream};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::mpsc;

    let (reply_tx, _reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let reply_count = Arc::new(AtomicUsize::new(0));
    let reply_count_route = reply_count.clone();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"shell","resources":["echo SAFE"]}}"#,
                        ),
                    )])
                    .chain(stream::pending());
                    Sse::new(events).keep_alive(KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                let reply_count = reply_count_route.clone();
                async move {
                    reply_count.fetch_add(1, Ordering::SeqCst);
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 run it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };

    core.interrupt(agent.id, main);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let outcome = core.approval_outcome(&req.cc_request_id).unwrap();
        let done = outcome.is_some();
        if done || tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let recorded = core.approval_outcome(&req.cc_request_id).unwrap();
    if let Some(outcome) = recorded {
        assert_ne!(
            outcome.state,
            ApprovalOutcomeState::Allowed,
            "interrupt must not auto-allow"
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        reply_count.load(Ordering::SeqCst) <= 1,
        "no duplicate permission execution"
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_disconnect_fails_closed_through_real_orchestrator() {
    use axum::response::sse::{Event, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::stream;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (reply_tx, _reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"shell","resources":["echo SAFE"]}}"#,
                        ),
                    )]);
                    Sse::new(events)
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                async move {
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 run it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };

    server.abort();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if core.approval_outcome(&req.cc_request_id).unwrap().is_some()
            || tokio::time::Instant::now() > deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let recorded = core.approval_outcome(&req.cc_request_id).unwrap();
    if let Some(outcome) = recorded {
        assert_ne!(
            outcome.state,
            ApprovalOutcomeState::Allowed,
            "disconnect must not auto-allow"
        );
    } else {
        panic!("disconnect should fail closed with an approval outcome");
    }
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_replay_rejected_after_core_restart() {
    use axum::response::sse::{Event, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::stream;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let db_path = std::env::temp_dir().join(format!(
        "cc-strm612-restart-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&db_path);

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"shell","resources":["echo SAFE"]}}"#,
                        ),
                    )]);
                    Sse::new(events)
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                async move {
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let cfg = HostConfig {
        db: Some(db_path.clone()),
        ..HostConfig::default()
    };
    let (core, mut events) = Core::start(cfg, clock.clone()).await.unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 run it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };
    core.resolve_approval(&req.cc_request_id, "reject").unwrap();
    let _reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
        .await
        .expect("provider reply arrives")
        .expect("reply channel open");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if core.approval_outcome(&req.cc_request_id).unwrap().is_some()
            || tokio::time::Instant::now() > deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let recorded = core.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(recorded.state, ApprovalOutcomeState::Denied);
    core.shutdown().await;

    let clock2 = ManualClock::new(1_000_100);
    let cfg2 = HostConfig {
        db: Some(db_path.clone()),
        ..HostConfig::default()
    };
    let (core2, _events2) = Core::start(cfg2, clock2).await.unwrap();
    let err = core2
        .resolve_approval(&req.cc_request_id, "reject")
        .unwrap_err();
    assert!(
        err.contains("resolved")
            || err.contains("unknown")
            || err.contains("not pending")
            || err.contains("no longer pending"),
        "unexpected replay error after restart: {err}"
    );
    let recorded2 = core2.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(recorded2.state, ApprovalOutcomeState::Denied);
    core2.shutdown().await;
    let _ = std::fs::remove_file(&db_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_replay_after_restart_is_rejected_at_provider() {
    use axum::response::sse::{Event, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::stream;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::mpsc;

    let db_path = std::env::temp_dir().join(format!(
        "cc-strm612-provider-restart-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&db_path);

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let reply_count = Arc::new(AtomicUsize::new(0));
    let reply_count_route = reply_count.clone();
    let ask_count = Arc::new(AtomicUsize::new(0));
    let ask_count_route = ask_count.clone();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                let ask_count = ask_count_route.clone();
                async move {
                    prompted.notified().await;
                    ask_count.fetch_add(1, Ordering::SeqCst);
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"shell","resources":["echo SAFE"]}}"#,
                        ),
                    )]);
                    Sse::new(events)
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                let reply_count = reply_count_route.clone();
                async move {
                    reply_count.fetch_add(1, Ordering::SeqCst);
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let adapter = || {
        Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
            command: "opencode".into(),
            args: Vec::new(),
            env: Vec::new(),
            server_url: Some(format!("http://{addr}")),
            server_password: None,
            model: None,
        }))
    };
    let agent_spec = || AgentSpec {
        handle: "v2".into(),
        name: "V2".into(),
        adapter: adapter(),
        cwd: Some(std::env::temp_dir()),
        ping_answers: None,
    };

    let cfg = HostConfig {
        db: Some(db_path.clone()),
        ..HostConfig::default()
    };
    let (core, mut events) = Core::start(cfg, ManualClock::new(1_000_000)).await.unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let agent = core.add_agent(agent_spec(), main).await.unwrap();
    let _ping = core.post(hx.id, main, "@v2 run it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };
    core.resolve_approval(&req.cc_request_id, "reject").unwrap();
    let _reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
        .await
        .expect("provider reply arrives")
        .expect("reply channel open");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if core.approval_outcome(&req.cc_request_id).unwrap().is_some()
            || tokio::time::Instant::now() > deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let recorded = core.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(recorded.state, ApprovalOutcomeState::Denied);
    core.shutdown().await;

    let cfg2 = HostConfig {
        db: Some(db_path.clone()),
        ..HostConfig::default()
    };
    let (core2, mut events2) = Core::start(cfg2, ManualClock::new(1_000_100))
        .await
        .unwrap();
    let main2 = common::main_channel(&core2).await.id;
    let hx2 = core2.human("hx", "Hx", main2).await.unwrap();
    let agent2 = core2.add_agent(agent_spec(), main2).await.unwrap();
    let _ping2 = core2
        .post(hx2.id, main2, "@v2 run it again", None)
        .await
        .unwrap();

    // The provider replays the same permission.asked id into a fresh Core on
    // the same database. The recovered outcome must reject it: no ApprovalRequested
    // event, no second provider reply, and the original Denied outcome stands.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, events2.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(asked))) => {
                assert_ne!(
                    asked.cc_request_id, req.cc_request_id,
                    "replayed provider ask reopened an already-resolved approval after restart"
                );
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        ask_count.load(Ordering::SeqCst) >= 2,
        "the provider must actually replay the permission ask after restart"
    );
    assert_eq!(
        reply_count.load(Ordering::SeqCst),
        1,
        "restart replay must not send a second provider reply"
    );
    let recorded2 = core2.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(recorded2.state, ApprovalOutcomeState::Denied);
    let _ = agent2;
    core2.shutdown().await;
    let _ = std::fs::remove_file(&db_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_edit_action_mediated_through_real_orchestrator() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::{StreamExt, stream};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![
                        Ok::<_, std::convert::Infallible>(Event::default().data(
                            r#"{"type":"session.tool.input.ended","data":{"sessionID":"s1","id":"tool-1","input":{"path":"src/main.rs","old":"a","new":"b"}}}"#,
                        )),
                        Ok(Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"edit","resources":["src/main.rs"],"source":{"type":"tool","messageID":"m1","id":"tool-1"}}}"#,
                        )),
                    ])
                    .chain(stream::pending());
                    Sse::new(events).keep_alive(KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                async move {
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 edit it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };
    assert_eq!(req.operation.action.as_ref(), "edit");
    core.resolve_approval(&req.cc_request_id, "once").unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
        .await
        .expect("provider reply arrives")
        .expect("reply channel open");
    assert_eq!(reply["decision"], "once");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if core.approval_outcome(&req.cc_request_id).unwrap().is_some()
            || tokio::time::Instant::now() > deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let outcome = core.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(outcome.state, ApprovalOutcomeState::Allowed);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_write_action_mediated_through_real_orchestrator() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::{StreamExt, stream};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"write","resources":["src/main.rs"],"source":{"type":"tool","messageID":"m1","id":"tool-1"}}}"#,
                        ),
                    )])
                    .chain(stream::pending());
                    Sse::new(events).keep_alive(KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                async move {
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 write it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };
    assert_eq!(req.operation.action.as_ref(), "write");
    core.resolve_approval(&req.cc_request_id, "once").unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
        .await
        .expect("provider reply arrives")
        .expect("reply channel open");
    assert_eq!(reply["decision"], "once");
    let outcome_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if core.approval_outcome(&req.cc_request_id).unwrap().is_some()
            || tokio::time::Instant::now() > outcome_deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let outcome = core.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(outcome.state, ApprovalOutcomeState::Allowed);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_read_action_mediated_through_real_orchestrator() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::{StreamExt, stream};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"read","resources":["src/main.rs"],"source":{"type":"tool","messageID":"m1","id":"tool-1"}}}"#,
                        ),
                    )])
                    .chain(stream::pending());
                    Sse::new(events).keep_alive(KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                async move {
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 read it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };
    assert_eq!(req.operation.action.as_ref(), "read");
    core.resolve_approval(&req.cc_request_id, "once").unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
        .await
        .expect("provider reply arrives")
        .expect("reply channel open");
    assert_eq!(reply["decision"], "once");
    let outcome_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if core.approval_outcome(&req.cc_request_id).unwrap().is_some()
            || tokio::time::Instant::now() > outcome_deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let outcome = core.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(outcome.state, ApprovalOutcomeState::Allowed);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_no_permission_asked_is_unmediated_no_approval_requested() {
    use axum::response::sse::{Event, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::stream;
    use std::sync::Arc;

    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"session.tool.input.ended","data":{"sessionID":"s1","id":"tool-1","input":{"command":"echo SAFE"}}}"#,
                        ),
                    )]);
                    Sse::new(events)
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 run it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => {
                panic!("unexpected approval request: {req:?}")
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_mcp_action_mediated_through_real_orchestrator() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::{StreamExt, stream};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"mcp","resources":["server.tool"],"source":{"type":"tool","messageID":"m1","id":"tool-1"}}}"#,
                        ),
                    )])
                    .chain(stream::pending());
                    Sse::new(events).keep_alive(KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                async move {
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 use mcp", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };
    assert_eq!(req.operation.action.as_ref(), "mcp");
    core.resolve_approval(&req.cc_request_id, "once").unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
        .await
        .expect("provider reply arrives")
        .expect("reply channel open");
    assert_eq!(reply["decision"], "once");
    let outcome_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if core.approval_outcome(&req.cc_request_id).unwrap().is_some()
            || tokio::time::Instant::now() > outcome_deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let outcome = core.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(outcome.state, ApprovalOutcomeState::Allowed);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_delete_action_mediated_through_real_orchestrator() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::{StreamExt, stream};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"delete","resources":["src/main.rs"],"source":{"type":"tool","messageID":"m1","id":"tool-1"}}}"#,
                        ),
                    )])
                    .chain(stream::pending());
                    Sse::new(events).keep_alive(KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(move |Json(body): Json<serde_json::Value>| {
                let reply_tx = reply_tx.clone();
                async move {
                    let _ = reply_tx.send(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 delete it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };
    assert_eq!(req.operation.action.as_ref(), "delete");
    core.resolve_approval(&req.cc_request_id, "once").unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
        .await
        .expect("provider reply arrives")
        .expect("reply channel open");
    assert_eq!(reply["decision"], "once");
    let outcome_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if core.approval_outcome(&req.cc_request_id).unwrap().is_some()
            || tokio::time::Instant::now() > outcome_deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let outcome = core.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(outcome.state, ApprovalOutcomeState::Allowed);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_v2_permission_reply_delivery_failure_records_failed_through_real_orchestrator() {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use futures::{StreamExt, stream};
    use std::sync::Arc;

    let prompted = Arc::new(tokio::sync::Notify::new());
    let sse_ready = prompted.clone();
    let app = Router::new()
        .route("/api/session", post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }))
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put(|| async { Json(serde_json::json!({ "data": {} })) }),
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
                    let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"shell","resources":["echo SAFE"],"source":{"type":"tool","messageID":"m1","id":"tool-1"}}}"#,
                        ),
                    )])
                    .chain(stream::pending());
                    Sse::new(events).keep_alive(KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/session/s1/permission/p1/reply",
            post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let clock = ManualClock::new(1_000_000);
    let (core, mut events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = common::main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: Some(format!("http://{addr}")),
        server_password: None,
        model: None,
    }));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "v2".into(),
                name: "V2".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    let _ping = core.post(hx.id, main, "@v2 run it", None).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let req = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(req))) if req.agent == agent.id => break *req,
            Ok(Some(_)) => {}
            other => panic!("no approval request: {other:?}"),
        }
    };
    core.resolve_approval(&req.cc_request_id, "once").unwrap();

    let outcome_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if core.approval_outcome(&req.cc_request_id).unwrap().is_some()
            || tokio::time::Instant::now() > outcome_deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let outcome = core.approval_outcome(&req.cc_request_id).unwrap().unwrap();
    assert_eq!(outcome.state, ApprovalOutcomeState::Failed);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("reply"),
        "expected reply failure context, got {:?}",
        outcome.error
    );
    core.shutdown().await;
}
