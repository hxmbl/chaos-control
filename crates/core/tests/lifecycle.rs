//! STRM-686: persisted global + per-chat lifecycle.
//!
//! These tests exercise the real engine wake path. Existing scenario tests
//! activate their agents in the shared harness; here we set lifecycle state
//! explicitly to check precedence and persistence.

mod common;

use std::sync::Arc;

use chaos_core::{
    acp::{AcpAdapter, FnTransport},
    fake::{self, FakeAgent, Script},
    host::{AgentSpec, Core, HostConfig, ManualClock},
    lifecycle::{DeliveryDecision, LifecycleState, MembershipState},
    types::*,
};
use common::*;
use serde_json::json;

fn empty_script() -> Script {
    script(json!({"turns": []}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_sleeping_skips_normal_chat_but_an_explicit_ping_wakes() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", empty_script()).await;
    h.core
        .set_agent_lifecycle(bot.id, LifecycleState::Sleeping, true)
        .await
        .unwrap();

    assert_eq!(
        h.core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Sleeping
    );
    assert!(h.core.agent_forced_sleep(bot.id).await.unwrap());
    // Normal chat on a sleeping agent is not notified.
    assert_eq!(
        h.core
            .effective_delivery(bot.id, h.main, WakeReason::Broadcast)
            .await
            .unwrap(),
        DeliveryDecision::Skip
    );
    // An explicit ping wakes the sleeping agent.
    assert_eq!(
        h.core
            .effective_delivery(bot.id, h.main, WakeReason::Mention)
            .await
            .unwrap(),
        DeliveryDecision::WakeAgent
    );

    // Ordinary chatter while sleeping wakes nobody.
    h.say("just thinking out loud").await;
    h.quiesce().await;
    assert!(
        h.prompts("bot").is_empty(),
        "no notification on normal chat"
    );
    assert_eq!(
        h.core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Sleeping
    );

    // A ping wakes the agent and delivers.
    h.say("@bot are you there?").await;
    h.quiesce().await;
    assert_eq!(h.prompts("bot").len(), 1, "pinged agent wakes up");
    assert_eq!(
        h.core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Active,
        "comes out of sleeping"
    );
    assert!(
        !h.core.agent_forced_sleep(bot.id).await.unwrap(),
        "a ping clears user-forced sleep"
    );

    // Once awake, it is notified on every chat...
    let before = h.prompts("bot").len();
    h.say("hello everyone").await;
    h.quiesce().await;
    assert_eq!(h.prompts("bot").len(), before + 1, "notified on every chat");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unresponsive_notifies_nothing_until_pinged() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", empty_script()).await;
    h.core
        .set_agent_lifecycle(bot.id, LifecycleState::Unresponsive, false)
        .await
        .unwrap();

    // Non-ping: nothing is notified in error mode.
    assert_eq!(
        h.core
            .effective_delivery(bot.id, h.main, WakeReason::Broadcast)
            .await
            .unwrap(),
        DeliveryDecision::Skip
    );
    // Explicit ping: the harness tries again.
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
            DeliveryDecision::RetryUnresponsive
        );
    }

    // Normal chat in error mode notifies nothing.
    h.say("hello room").await;
    h.quiesce().await;
    assert!(h.prompts("bot").is_empty(), "nothing until pinged");
    assert_eq!(
        h.core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Unresponsive
    );

    // A ping makes the harness try again; with nothing else wrong the
    // agent leaves error mode.
    h.say("@bot wake up").await;
    h.quiesce().await;
    assert_eq!(h.prompts("bot").len(), 1, "pinged in error mode retries");
    assert_eq!(
        h.core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Active,
        "not re-entering error mode when the retry works"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_sleeping_blocks_only_that_chat_and_a_ping_wakes_it() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", empty_script()).await;
    let project = project_id();
    let side = h.core.channel(&project, "side").await.unwrap();
    h.core.join(bot.id, side.id).await.unwrap();
    let side_chat = h
        .core
        .chats()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.targets.iter().any(|t| t.channel == side.id))
        .unwrap();

    h.core
        .set_chat_membership_lifecycle(side_chat.id, bot.id, MembershipState::Sleeping, true)
        .await
        .unwrap();
    assert_eq!(
        h.core
            .chat_membership_lifecycle(side_chat.id, bot.id)
            .await
            .unwrap(),
        MembershipState::Sleeping
    );
    assert!(
        h.core
            .chat_membership_forced_sleep(side_chat.id, bot.id)
            .await
            .unwrap()
    );

    // Ordinary messages in the sleeping chat are dropped.
    h.core
        .post(h.hx.id, side.id, "ordinary chatter", None)
        .await
        .unwrap();
    h.quiesce().await;
    assert!(h.prompts("bot").is_empty());

    // The other chat is unaffected.
    h.say("ordinary chatter in main").await;
    h.quiesce().await;
    assert_eq!(h.prompts("bot").len(), 1);

    // An explicit ping in the sleeping chat wakes the membership, not the
    // agent globally.
    assert_eq!(
        h.core
            .effective_delivery(bot.id, side.id, WakeReason::Mention)
            .await
            .unwrap(),
        DeliveryDecision::WakeMembership
    );
    h.core
        .post(h.hx.id, side.id, "@bot check this", None)
        .await
        .unwrap();
    h.quiesce().await;
    assert_eq!(
        h.core
            .chat_membership_lifecycle(side_chat.id, bot.id)
            .await
            .unwrap(),
        MembershipState::Active
    );
    assert!(
        !h.core
            .chat_membership_forced_sleep(side_chat.id, bot.id)
            .await
            .unwrap()
    );
    assert_eq!(
        h.core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Active
    );
    assert_eq!(h.prompts("bot").len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_activation_does_not_wake_a_sleeping_membership() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", empty_script()).await;
    let chat = h.core.chats().await.unwrap()[0].id;
    h.core
        .set_chat_membership_lifecycle(chat, bot.id, MembershipState::Sleeping, true)
        .await
        .unwrap();
    h.core
        .set_agent_lifecycle(bot.id, LifecycleState::Sleeping, true)
        .await
        .unwrap();

    h.core
        .set_agent_lifecycle(bot.id, LifecycleState::Active, false)
        .await
        .unwrap();

    assert_eq!(
        h.core
            .chat_membership_lifecycle(chat, bot.id)
            .await
            .unwrap(),
        MembershipState::Sleeping
    );
    assert_eq!(
        h.core
            .effective_delivery(bot.id, h.main, WakeReason::Broadcast)
            .await
            .unwrap(),
        DeliveryDecision::Skip
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_sleep_survives_a_core_restart() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cc-lifecycle-restart-{nanos}"));
    let _ = std::fs::remove_dir_all(&dir);
    let config = || HostConfig {
        db: Some(dir.clone()),
        ..HostConfig::default()
    };

    let fresh = FakeAgent::new(empty_script());
    let spec = || {
        let f = fresh.clone();
        AgentSpec {
            handle: "bot".into(),
            name: "Bot".into(),
            adapter: Arc::new(AcpAdapter::new(Arc::new(FnTransport(
                "fake:bot".into(),
                move || fake::in_process(f.clone()),
            )))),
            cwd: Some(std::env::temp_dir()),
            ping_answers: None,
        }
    };

    let (core, _events) = Core::start(config(), ManualClock::new(T0)).await.unwrap();
    let main = common::main_channel(&core).await.id;
    let bot = core.add_agent(spec(), main).await.unwrap();
    // New agents are globally Sleeping even on a file-backed store.
    assert_eq!(
        core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Sleeping
    );
    core.set_agent_lifecycle(bot.id, LifecycleState::Sleeping, true)
        .await
        .unwrap();
    core.shutdown().await;

    let (restarted, _events) = Core::start(config(), ManualClock::new(T0 + 1))
        .await
        .unwrap();
    let main = common::main_channel(&restarted).await.id;
    let members = restarted.members(main).await.unwrap();
    let bot = members
        .into_iter()
        .find(|m| m.handle.as_ref() == "bot")
        .expect("agent survived restart");
    assert_eq!(
        restarted.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Sleeping
    );
    assert!(restarted.agent_forced_sleep(bot.id).await.unwrap());
    // Forced sleep persists across restart; an explicit ping still wakes it.
    assert_eq!(
        restarted
            .effective_delivery(bot.id, main, WakeReason::Mention)
            .await
            .unwrap(),
        DeliveryDecision::WakeAgent
    );
    assert_eq!(
        restarted
            .effective_delivery(bot.id, main, WakeReason::Broadcast)
            .await
            .unwrap(),
        DeliveryDecision::Skip
    );
    restarted.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Error mode (runtime `Unresponsive`, e.g. harness missing): notified of
/// nothing until pinged; a ping makes the harness try again and it may
/// re-enter error mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_mode_gates_everything_until_pinged_and_ping_retries() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", empty_script()).await;

    // Enter error mode: harness missing.
    h.core
        .with_engine(move |e, _| {
            e.set_probe(
                bot.id,
                h.main,
                &chaos_core::adapter::Probe::Missing {
                    why: std::sync::Arc::from("command not found"),
                },
            );
        })
        .await
        .unwrap();
    h.quiesce().await;
    assert_eq!(
        h.core.status(bot.id, h.main).await.unwrap().glow,
        chaos_core::types::Glow::Unresponsive
    );

    // Normal chat in error mode notifies nothing.
    h.say("hello room").await;
    h.quiesce().await;
    assert!(h.prompts("bot").is_empty(), "nothing until pinged");

    // A sleeping-style normal chat update doesn't even queue.
    assert_eq!(
        h.core
            .effective_delivery(bot.id, h.main, WakeReason::Broadcast)
            .await
            .unwrap(),
        DeliveryDecision::Skip
    );

    // Pinged: harness tries again. With the failure gone, error mode clears.
    h.say("@bot are you there?").await;
    h.quiesce().await;
    assert_eq!(h.prompts("bot").len(), 1, "pinged -> harness retries");
    assert_ne!(
        h.core.status(bot.id, h.main).await.unwrap().glow,
        chaos_core::types::Glow::Unresponsive,
        "retry that works leaves error mode"
    );
}

/// A sleeping agent can still read the chat; it just isn't notified.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sleeping_agent_can_check_chat_but_is_not_notified() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", empty_script()).await;
    let chat = h.core.chats().await.unwrap()[0].id;
    h.core
        .set_chat_membership_lifecycle(chat, bot.id, MembershipState::Sleeping, true)
        .await
        .unwrap();

    h.say("first update").await;
    h.say("second update").await;
    h.quiesce().await;
    assert!(h.prompts("bot").is_empty(), "not pinged on normal chat");

    // It can check the chat itself.
    let history = h.core.history(h.main, 10).await.unwrap();
    assert!(
        history.iter().any(|m| m.body.as_ref() == "first update"),
        "sleeping agent can read the chat"
    );
    assert_eq!(
        h.core
            .chat_membership_lifecycle(chat, bot.id)
            .await
            .unwrap(),
        MembershipState::Sleeping,
        "reading chat does not wake it"
    );
}
