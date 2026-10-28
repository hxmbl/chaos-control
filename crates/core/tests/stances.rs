//! Refusal and dissent as peer actions (STRM-343): a refusal answers a reply
//! obligation as `refused` (no fallback, no retry), both are recorded with
//! who, what, the model and the reasoning, and agents see them again through
//! `chat_context` / `chat_read`.

mod common;

use chaos_core::{mcp, store, types::*};
use common::*;
use serde_json::{Value, json};

fn refuse(reason: &str) -> Value {
    json!({"mcp": {"tool": "chat_refuse", "args": {
        "message_id": "$TRIGGER", "reason": reason, "model": "test-model-1",
        "client_request_id": "refuse-$N"}}})
}

async fn tool(h: &Harness, agent: MemberId, name: &'static str, args: Value) -> Value {
    let scope = mcp::Scope {
        agent,
        channel: h.main,
        epoch: 0,
    };
    h.core
        .with_engine(move |e, now| mcp::call_tool(e, now, scope, name, &args))
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("{name} failed: {}", e.0))
}

async fn settled(h: &mut Harness, n: i64) {
    h.until("batches settled", move |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'settled'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == n
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusal_answers_the_ping_with_no_fallback_and_no_retry() {
    let mut h = Harness::new().await;
    // Refuses, then ends the turn with final text that would otherwise be
    // posted as the fallback reply.
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                refuse("If you want it badly enough, do it yourself."),
                {"text": "I said no in the channel."}
            ]}]})),
        )
        .await;
    let ping = h.say("@bot rewrite the whole store in Go").await;
    let m = h.wait_message("refusal", Some(bot.id)).await;
    assert_eq!(m.origin, Origin::Refusal);
    assert_eq!(m.reply_to, Some(ping.id));
    assert_eq!(&*m.body, "If you want it badly enough, do it yourself.");
    settled(&mut h, 1).await;
    h.quiesce().await;

    assert_eq!(
        h.obligation(bot.id, ping.id).await,
        Some(ObligationState::Refused)
    );
    assert_eq!(h.by(bot.id).await.len(), 1, "no fallback after a refusal");
    let st = h.core.status(bot.id, h.main).await.unwrap();
    assert_ne!(st.glow, Glow::Unresponsive, "a refusal is not a failure");
    h.advance(10 * 60 * 1000).await;
    assert_eq!(h.prompts("bot").len(), 1, "a refusal is never retried");

    // Recorded with who, what, model and reasoning, and the runtime.
    let stance = h
        .core
        .stance_of(m.id)
        .await
        .unwrap()
        .expect("stance recorded");
    assert_eq!(stance.kind, StanceKind::Refusal);
    assert_eq!(stance.member, bot.id);
    assert_eq!(stance.target, ping.id);
    assert_eq!(stance.model.as_deref(), Some("test-model-1"));
    assert!(
        stance
            .runtime
            .as_deref()
            .is_some_and(|r| r.contains("fake-acp-agent")),
        "runtime from the handshake: {:?}",
        stance.runtime
    );
    let resolved: (i64, String) = h
        .core
        .with_engine(move |e, _| {
            e.conn()
                .query_row(
                    "SELECT resolved_by, resolved_kind FROM obligations WHERE message_id = ?1",
                    [ping.id.0],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!(resolved, (m.id.0, "refusal".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stances_survive_in_context_and_read_and_the_prompt() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [
                {"steps": [refuse("Not worth it; the flake is upstream.")]},
                {"steps": [send_reply("ok")]}
            ]})),
        )
        .await;
    let ping = h.say("@bot pin every dependency").await;
    let refusal = h.wait_message("refusal", Some(bot.id)).await;
    settled(&mut h, 1).await;

    // The human disagrees, as a peer: a dissent replying to the refusal.
    let dissent = h
        .core
        .stance(
            h.hx.id,
            h.main,
            StanceKind::Dissent,
            refusal.id,
            "I think it is ours, @bot. Look at the lockfile.",
        )
        .await
        .unwrap();
    assert_eq!(dissent.origin, Origin::Dissent);
    assert_eq!(dissent.reply_to, Some(refusal.id));
    // It replies to (and mentions) the bot: the bot is woken as usual.
    let prompts = h.wait_prompts("bot", 2).await;
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[1].contains("[dissent]"),
        "the prompt labels the dissent: {}",
        prompts[1]
    );
    assert!(prompts[1].contains("[refused]") || prompts[1].contains(&refusal.id.to_string()));
    settled(&mut h, 2).await;

    let ctx = tool(&h, bot.id, "chat_context", json!({})).await;
    let stances = ctx["stances"].as_array().expect("stances in chat_context");
    assert_eq!(stances.len(), 2);
    assert_eq!(stances[0]["kind"], "refusal");
    assert_eq!(stances[0]["by"]["handle"], "bot");
    assert_eq!(stances[0]["target"], json!(ping.id));
    assert_eq!(stances[0]["model"], "test-model-1");
    assert_eq!(stances[0]["reason"], "Not worth it; the flake is upstream.");
    assert_eq!(stances[1]["kind"], "dissent");
    assert_eq!(stances[1]["by"]["handle"], "hx");
    assert!(stances[1].get("model").is_none(), "humans report no model");

    let read = tool(&h, bot.id, "chat_read", json!({"after_seq": 0})).await;
    let msgs = read["messages"].as_array().unwrap();
    let r = msgs
        .iter()
        .find(|m| m["id"] == json!(refusal.id))
        .expect("refusal in chat_read");
    assert_eq!(r["origin"], "refusal");
    assert_eq!(r["stance"]["kind"], "refusal");
    assert_eq!(r["stance"]["target_seq"], json!(ping.seq));
    let plain = msgs.iter().find(|m| m["id"] == json!(ping.id)).unwrap();
    assert!(plain.get("stance").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dissent_from_an_agent_counts_as_its_reply() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                {"mcp": {"tool": "chat_dissent", "args": {
                    "message_id": "$TRIGGER",
                    "reason": "I'd rather not mock the clock; the test should own time.",
                    "client_request_id": "d-1"}}},
                {"text": "private summary"}
            ]}]})),
        )
        .await;
    let ping = h.say("@bot mock the clock in the scenario tests").await;
    let m = h.wait_message("dissent", Some(bot.id)).await;
    assert_eq!(m.origin, Origin::Dissent);
    settled(&mut h, 1).await;
    h.quiesce().await;
    assert_eq!(
        h.obligation(bot.id, ping.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(h.by(bot.id).await.len(), 1, "no fallback after a dissent");
    let st = h.core.stance_of(m.id).await.unwrap().unwrap();
    assert_eq!(st.kind, StanceKind::Dissent);
    assert_eq!(st.model, None, "model not reported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_needs_a_reason_and_a_target_in_this_channel() {
    let mut h = Harness::new().await;
    let bot = h
        .agent("bot", script(json!({"turns": [{"steps": []}]})))
        .await;
    let ping = h.say("hello").await;
    let scope = mcp::Scope {
        agent: bot.id,
        channel: h.main,
        epoch: 0,
    };
    let errs = h
        .core
        .with_engine(move |e, now| {
            let no_reason = mcp::call_tool(
                e,
                now,
                scope,
                "chat_refuse",
                &json!({"message_id": ping.id, "reason": "  ", "client_request_id": "x"}),
            )
            .err()
            .map(|e| e.0);
            let no_target = mcp::call_tool(
                e,
                now,
                scope,
                "chat_refuse",
                &json!({"message_id": 999_999, "reason": "no", "client_request_id": "y"}),
            )
            .err()
            .map(|e| e.0);
            let stances = store::recent_stances(e.conn(), scope.channel, 10)
                .unwrap()
                .len();
            (no_reason, no_target, stances)
        })
        .await
        .unwrap();
    assert!(errs.0.is_some_and(|e| e.contains("reason")));
    assert!(errs.1.is_some());
    assert_eq!(errs.2, 0, "nothing recorded");
    h.quiesce().await;
}
