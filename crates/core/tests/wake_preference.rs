//! STRM-657: an agent's own normal-message preference.
//!
//! The preference only gates ordinary broadcasts. An explicit ping and a
//! user-forced sleep outrank it (`lifecycle::delivery_decision`), so these
//! tests cover the store round trip, the delivery decision, and the real wake
//! path end to end.

mod common;

use std::sync::Arc;

use chaos_core::{
    acp::{AcpAdapter, FnTransport},
    fake::{self, FakeAgent, Script},
    host::MemberSpec,
    lifecycle::DeliveryDecision,
    types::*,
};
use common::*;
use serde_json::json;

fn empty_script() -> Script {
    script(json!({"turns": []}))
}

/// A roster model member (the shape STRM-657 applies to) with a live session on
/// #main, activated and tracked in the harness so `prompts`/`say` work like
/// the other scenario tests.
async fn model_member(h: &mut Harness, name: &str) -> Member {
    let fake = FakeAgent::new(empty_script());
    let f = fake.clone();
    let adapter = Arc::new(AcpAdapter::new(Arc::new(FnTransport(
        format!("fake:{name}"),
        move || fake::in_process(f.clone()),
    ))));
    let added = h
        .core
        .add_model_member(
            MemberSpec {
                harness: "fake".into(),
                model: None,
                model_name: name.into(),
                adapter: adapter.clone(),
                cwd: Some(std::env::temp_dir()),
            },
            h.main,
        )
        .await
        .unwrap();
    // New agents start globally Sleeping (STRM-670); these tests are about
    // delivery, so activate the way the shared harness does.
    activate_agent(&h.core, added.member.id).await;
    h.agents.insert(
        added.member.handle.to_string(),
        (added.member.clone(), fake, adapter),
    );
    added.member
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcasts_deliver_until_the_agent_opts_out() {
    let mut h = Harness::new().await;
    let bot = model_member(&mut h, "Wake Bot").await;
    let handle = bot.handle.to_string();

    // The column defaults to 1, so existing and new roster members alike keep
    // today's behaviour until they choose otherwise.
    assert_eq!(
        h.core.member_wake_normal_messages(bot.id).await.unwrap(),
        Some(true)
    );
    h.say("ordinary chat").await;
    h.quiesce().await;
    assert_eq!(
        h.prompts(&handle).len(),
        1,
        "the default preference receives broadcasts"
    );

    h.core
        .set_member_wake_normal_messages(bot.id, false)
        .await
        .unwrap();
    assert_eq!(
        h.core.member_wake_normal_messages(bot.id).await.unwrap(),
        Some(false)
    );

    h.say("still just chat").await;
    h.quiesce().await;
    assert_eq!(
        h.prompts(&handle).len(),
        1,
        "an opted-out agent skips ordinary broadcasts"
    );

    h.say(&format!("@{handle} hello")).await;
    h.quiesce().await;
    assert_eq!(
        h.prompts(&handle).len(),
        2,
        "an explicit ping still wakes it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_preference_gates_broadcasts_but_not_pings() {
    let mut h = Harness::new().await;
    let bot = model_member(&mut h, "Wake Bot").await;

    for reason in [
        WakeReason::Broadcast,
        WakeReason::Mention,
        WakeReason::ReplyToMe,
        WakeReason::Everyone,
    ] {
        assert_eq!(
            h.core
                .effective_delivery(bot.id, h.main, reason)
                .await
                .unwrap(),
            DeliveryDecision::Deliver,
            "{reason:?} delivers while the preference is on"
        );
    }

    h.core
        .set_member_wake_normal_messages(bot.id, false)
        .await
        .unwrap();
    for reason in [
        WakeReason::Mention,
        WakeReason::ReplyToMe,
        WakeReason::Everyone,
    ] {
        assert_eq!(
            h.core
                .effective_delivery(bot.id, h.main, reason)
                .await
                .unwrap(),
            DeliveryDecision::Deliver,
            "the preference never gates an explicit ping ({reason:?})"
        );
    }
    assert_eq!(
        h.core
            .effective_delivery(bot.id, h.main, WakeReason::Broadcast)
            .await
            .unwrap(),
        DeliveryDecision::Skip
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_roster_model_members_have_a_preference() {
    let mut h = Harness::new().await;
    // A plain agent (not in the roster's model_members) has no preference row.
    let bot = h.agent("bot", empty_script()).await;

    assert_eq!(
        h.core.member_wake_normal_messages(bot.id).await.unwrap(),
        None
    );
    let err = h
        .core
        .set_member_wake_normal_messages(bot.id, false)
        .await
        .unwrap_err();
    assert!(
        err.contains("roster model member"),
        "the refusal is surfaced honestly, got: {err}"
    );

    // No stored preference is not silence: it keeps the default, deliver.
    h.say("ordinary chat").await;
    h.quiesce().await;
    assert_eq!(
        h.prompts("bot").len(),
        1,
        "an agent without a row still hears broadcasts"
    );
}
