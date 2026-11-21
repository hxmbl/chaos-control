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
