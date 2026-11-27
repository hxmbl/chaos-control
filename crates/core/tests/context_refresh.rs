//! STRM-688: Stop and check chat as a runtime operation.
//!
//! Interruption comes first; the high-priority instruction goes straight to
//! the live session, never through the message graph. Delivery is reported as
//! delivery, not as proof the agent refreshed its context.

mod common;

use std::time::Duration;

use chaos_core::lifecycle::{InterruptOutcome, LifecycleScope, RefreshOutcome};
use chaos_core::types::ObligationState;
use common::*;
use serde_json::json;

fn slow_then_instruction_then_resume() -> chaos_core::fake::Script {
    script(json!({"turns": [
        {"steps": [{"gate": "never"}]},
        {"steps": [{"text": "Checked the unread history."}]},
        {"steps": [send_reply("resumed with fresh context")]}
    ]}))
}

async fn wait_prompt_containing(h: &Harness, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(p) = h.prompts("bot").into_iter().find(|p| p.contains(needle)) {
            return p;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a prompt containing {needle:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_cancels(h: &Harness, n: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if h.fake("bot").observed.lock().unwrap().cancels >= n {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {n} cancel(s)"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_precedes_instruction_and_creates_no_chat_work() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", slow_then_instruction_then_resume()).await;
    let first = h.say("@bot look into it").await;
    h.wait_prompts("bot", 1).await;
    let history_before = h.history().await.len();
    let batches_before = h.count("SELECT COUNT(*) FROM batches").await;
    let obligations_before = h.count("SELECT COUNT(*) FROM obligations").await;

    let outcome = h
        .core
        .stop_and_check_chat(bot.id, LifecycleScope::Global)
        .await
        .unwrap();
    assert_eq!(
        outcome.interruptions,
        vec![(h.main, InterruptOutcome::Confirmed)]
    );
    assert_eq!(outcome.instructed.len(), 1);
    assert_eq!(outcome.instructed[0].0, h.core.chats().await.unwrap()[0].id);
    assert_eq!(outcome.instructed[0].1, h.main);
    assert!(outcome.no_session.is_empty());

    let prompt = wait_prompt_containing(&h, "runtime instruction").await;
    assert!(prompt.contains("high priority"), "prompt: {prompt}");
    assert!(prompt.contains("last read point"), "prompt: {prompt}");
    assert!(
        !prompt.contains(&format!("id:{}", first.id)),
        "the instruction is not an ordinary trigger for the old message"
    );
    wait_cancels(&h, 1).await;

    // Nothing was posted, batched or made into an obligation by the control.
    assert_eq!(h.history().await.len(), history_before);
    assert_eq!(
        h.count("SELECT COUNT(*) FROM batches").await,
        batches_before
    );
    assert_eq!(
        h.count("SELECT COUNT(*) FROM obligations").await,
        obligations_before
    );

    let refresh = h
        .core
        .context_refresh(bot.id, h.main)
        .await
        .unwrap()
        .expect("refresh state is exposed");
    assert!(refresh.instruction_delivered);
    assert!(refresh.head_seq >= refresh.read_through);
    assert_eq!(refresh.head_seq, 1, "one message in the channel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_obligations_survive_the_refresh_and_resume() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", slow_then_instruction_then_resume()).await;
    let first = h.say("@bot look into it").await;
    h.wait_prompts("bot", 1).await;
    let second = h.say("@bot and one more thing").await;
    h.until("second ping pending", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'pending'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            == 1
    })
    .await;

    h.core
        .stop_and_check_chat(bot.id, LifecycleScope::Global)
        .await
        .unwrap();
    wait_prompt_containing(&h, "runtime instruction").await;
    let reply = h.wait_message("resume after refresh", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(second.id));
    assert_eq!(&*reply.body, "resumed with fresh context");
    assert_eq!(
        h.obligation(bot.id, first.id).await,
        Some(ObligationState::Unmet),
        "the interrupted turn was not silently retried"
    );
    assert_eq!(
        h.obligation(bot.id, second.id).await,
        Some(ObligationState::Satisfied),
        "the pending obligation was not lost or duplicated"
    );
    assert_eq!(h.prompts("bot").len(), 3, "one turn per hand-off");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_scope_only_refreshes_that_chat() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", slow_then_instruction_then_resume()).await;
    let side = h.core.channel(&project_id(), "side").await.unwrap();
    h.core.join(h.hx.id, side.id).await.unwrap();
    h.core.join(bot.id, side.id).await.unwrap();
    let side_chat = h
        .core
        .chats()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.targets.iter().any(|t| t.channel == side.id))
        .unwrap();
    h.say("@bot main work").await;
    h.core
        .post(h.hx.id, side.id, "@bot side work", None)
        .await
        .unwrap();
    h.wait_prompts("bot", 2).await;

    let outcome = h
        .core
        .stop_and_check_chat(bot.id, LifecycleScope::Chat(side_chat.id))
        .await
        .unwrap();
    assert_eq!(outcome.interruptions.len(), 1);
    assert_eq!(outcome.interruptions[0].0, side.id);
    assert_eq!(outcome.instructed.len(), 1);
    assert_eq!(outcome.instructed[0].0, side_chat.id);
    assert!(
        h.core
            .context_refresh(bot.id, h.main)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.core
            .context_refresh(bot.id, side.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_live_session_reports_no_instruction() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", slow_then_instruction_then_resume()).await;
    let outcome: RefreshOutcome = h
        .core
        .stop_and_check_chat(bot.id, LifecycleScope::Global)
        .await
        .unwrap();
    assert_eq!(outcome.interruptions[0].1, InterruptOutcome::NoActiveTurn);
    assert!(outcome.instructed.is_empty());
    assert_eq!(outcome.no_session.len(), 1);
    assert!(h.prompts("bot").is_empty());
    assert!(
        h.core
            .context_refresh(bot.id, h.main)
            .await
            .unwrap()
            .is_none()
    );
}
