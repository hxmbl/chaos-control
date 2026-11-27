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
        activate_agent(&core, agent.id).await;

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
        activate_agent(&core, agent.id).await;
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
        activate_agent(&core, agent.id).await;
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
        activate_agent(&core, agent.id).await;
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
        activate_agent(&core, agent.id).await;
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
    use futures::{StreamExt, stream};
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
                    // Keep the stream open: EOF is a provider disconnect and
                    // would fail the pending approval closed before it can be
                    // resolved by this test.
                    Sse::new(events.chain(stream::pending()))
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
        activate_agent(&core, agent.id).await;
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
    use futures::{StreamExt, stream};
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
                    // Keep the stream open after the event. EOF is treated as
                    // a provider disconnect, which fails the request closed
                    // before this test can resolve it.
                    Sse::new(events.chain(stream::pending()))
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
    activate_agent(&core, agent.id).await;
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
    activate_agent(&core2, agent2.id).await;
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
        activate_agent(&core, agent.id).await;
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
        activate_agent(&core, agent.id).await;
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
        activate_agent(&core, agent.id).await;
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
        activate_agent(&core, agent.id).await;
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
        activate_agent(&core, agent.id).await;
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
        activate_agent(&core, agent.id).await;
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
        activate_agent(&core, agent.id).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_on_one_server_do_not_share_tool_calls_or_events() {
    use axum::extract::Path;
    use axum::response::sse::{Event, Sse};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use chaos_core::types::BatchState;
    use chaos_core::{
        host::{AgentSpec, Core, HostConfig, ManualClock},
        opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    };
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<String>(256);
    let streams = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let next_session_id = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Each session registers chat tools on the shared server. Record the name
    // and bearer token to prove the second session cannot replace the first.
    let registrations: Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = Router::new()
        .route(
            "/api/session",
            post({
                let next = next_session_id.clone();
                move || async move {
                    let n = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(json!({ "data": { "id": format!("s{}", n + 1) } }))
                }
            }),
        )
        .route(
            "/api/experimental/mcp/{server}",
            axum::routing::put({
                let registrations = registrations.clone();
                move |Path(server): Path<String>, Json(body): Json<serde_json::Value>| {
                    let registrations = registrations.clone();
                    async move {
                        // The bearer token lives in the MCP config body; the
                        // request's own Authorization is server basic auth.
                        let token = body["config"]["headers"]["Authorization"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        registrations.lock().unwrap().push((server, token));
                        Json(json!({ "data": {} }))
                    }
                }
            }),
        )
        .route(
            "/api/event",
            get({
                let tx = events_tx.clone();
                let streams = streams.clone();
                move || {
                    let rx = tx.subscribe();
                    streams.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async move {
                        Sse::new(futures::stream::unfold(rx, |mut rx| async move {
                            match rx.recv().await {
                                Ok(data) => Some((
                                    Ok::<_, std::convert::Infallible>(Event::default().data(data)),
                                    rx,
                                )),
                                Err(_) => None,
                            }
                        }))
                    }
                }
            }),
        )
        .route(
            "/api/session/{id}/prompt",
            post(|| async { Json(json!({ "data": {} })) }),
        )
        .route(
            "/api/session/{id}/permission/{permission}/reply",
            post({
                let reply_tx = reply_tx.clone();
                move |Path((session, permission)): Path<(String, String)>,
                      Json(body): Json<serde_json::Value>| {
                    let reply_tx = reply_tx.clone();
                    async move {
                        let _ = reply_tx.send(json!({
                            "session": session,
                            "permission": permission,
                            "body": body,
                        }));
                        axum::http::StatusCode::NO_CONTENT
                    }
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
    let agent_a = core
        .add_agent(
            AgentSpec {
                handle: "v2a".into(),
                name: "V2A".into(),
                adapter: adapter.clone(),
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
        activate_agent(&core, agent_a.id).await;
    let agent_b = core
        .add_agent(
            AgentSpec {
                handle: "v2b".into(),
                name: "V2B".into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
        activate_agent(&core, agent_b.id).await;

    let _ping_a = core.post(hx.id, main, "@v2a run it", None).await.unwrap();
    let _ping_b = core.post(hx.id, main, "@v2b run it", None).await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while streams.load(std::sync::atomic::Ordering::SeqCst) < 2
        || registrations.lock().unwrap().len() < 2
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "both sessions should connect and register chat tools"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let registrations = registrations.lock().unwrap().clone();
    assert_eq!(registrations.len(), 2);
    assert!(
        registrations
            .iter()
            .all(|(name, _)| name.starts_with("chaos-control-")),
        "registrations: {registrations:?}"
    );
    assert_ne!(
        registrations[0].0, registrations[1].0,
        "the second session must not overwrite the first session's MCP registration"
    );
    assert!(
        registrations
            .iter()
            .all(|(_, token)| token.starts_with("Bearer ") && token.len() > "Bearer ".len()),
        "each session must carry its own chat-tools token: {registrations:?}"
    );
    assert_ne!(
        registrations[0].1, registrations[1].1,
        "sessions must not share a chat-tools token"
    );

    // One shared broadcast: both adapters must drop the other session's frames.
    for data in [
        r#"{"type":"session.tool.input.ended","data":{"sessionID":"s1","id":"tool-1","input":{"command":"echo SAFE"}}}"#,
        r#"{"type":"session.tool.input.ended","data":{"sessionID":"s2","id":"tool-1","input":{"command":"echo OTHER"}}}"#,
        r#"{"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"shell","resources":["echo SAFE"],"source":{"type":"tool","messageID":"m1","id":"tool-1"}}}"#,
        r#"{"type":"permission.asked","data":{"id":"p2","sessionID":"s2","action":"shell","resources":["echo OTHER"],"source":{"type":"tool","messageID":"m2","id":"tool-1"}}}"#,
    ] {
        events_tx.send(data.to_owned()).unwrap();
    }

    let mut got: Vec<chaos_core::approval::ApprovalRequest> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while got.len() < 2 {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(CoreEvent::ApprovalRequested(r)))
                if r.agent == agent_a.id || r.agent == agent_b.id =>
            {
                got.push(*r);
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("core stopped; got {got:?}"),
            Err(_) => panic!("timed out waiting for approvals; got {got:?}"),
        }
    }
    let req_s1 = got
        .iter()
        .find(|r| &*r.session_id == "s1")
        .cloned()
        .expect("approval for session s1");
    let req_s2 = got
        .iter()
        .find(|r| &*r.session_id == "s2")
        .cloned()
        .expect("approval for session s2");
    let turn_s1 = req_s1.turn.expect("session 1 approval carries its turn");
    let turn_s2 = req_s2.turn.expect("session 2 approval carries its turn");

    // No tool-call theft: each session's approval carries its own tool call,
    // keyed by `tool-1` in both sessions, so an overwrite would show.
    assert_eq!(req_s1.operation.summary.as_deref(), Some("echo SAFE"));
    assert_eq!(
        req_s1.operation.arguments,
        Some(json!({ "command": "echo SAFE" }))
    );
    assert_eq!(req_s2.operation.summary.as_deref(), Some("echo OTHER"));
    assert_eq!(
        req_s2.operation.arguments,
        Some(json!({ "command": "echo OTHER" }))
    );

    // Observe the exact turn each approval belongs to. A newer queued batch
    // can already be in flight by the time the first one settles.
    let batch_state = |turn: chaos_core::types::BatchId| {
        core.with_engine(move |e, _| {
            e.store()
                .batch(turn)
                .unwrap()
                .map(|b| b.state)
                .unwrap_or(BatchState::Settled)
        })
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while batch_state(turn_s1).await.unwrap() != BatchState::Accepted
        || batch_state(turn_s2).await.unwrap() != BatchState::Accepted
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "both sessions should be in flight"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Events don't cross: session 1's success settles only session 1;
    // session 2's batch stays accepted until session 2's own event.
    events_tx
        .send(r#"{"type":"session.execution.succeeded","data":{"sessionID":"s1"}}"#.to_owned())
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while batch_state(turn_s1).await.unwrap() != BatchState::Settled {
        if tokio::time::Instant::now() > deadline {
            println!(
                "dbg: TIMED OUT turn_s1={:?} turn_s2={:?}",
                batch_state(turn_s1).await.unwrap(),
                batch_state(turn_s2).await.unwrap()
            );
            panic!("session 1 should settle");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        batch_state(turn_s2).await.unwrap(),
        BatchState::Accepted,
        "session 2 must not settle from session 1's events"
    );

    events_tx
        .send(r#"{"type":"session.execution.succeeded","data":{"sessionID":"s2"}}"#.to_owned())
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while batch_state(turn_s2).await.unwrap() != BatchState::Settled {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session 2 should settle"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    core.resolve_approval(&req_s1.cc_request_id, "once")
        .unwrap();
    core.resolve_approval(&req_s2.cc_request_id, "once")
        .unwrap();
    let mut replies = Vec::new();
    for _ in 0..2 {
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
            .await
            .expect("provider reply arrives")
            .expect("reply channel open");
        assert_eq!(reply["body"]["decision"], json!("once"));
        replies.push((
            reply["session"].as_str().unwrap().to_owned(),
            reply["permission"].as_str().unwrap().to_owned(),
        ));
    }
    replies.sort();
    assert_eq!(
        replies,
        vec![("s1".into(), "p1".into()), ("s2".into(), "p2".into())]
    );
    core.shutdown().await;
}
