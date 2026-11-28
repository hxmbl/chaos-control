//! Regression tests for the dispatch-gate / settle findings:
//!
//! - F1: lifecycle gates dispatch, not just trigger creation. Queued work is
//!   not pumped while the agent — or its chat membership in the target chat —
//!   is asleep, and waking re-pumps work queued while asleep.
//! - F2: a runtime control settles the batch captured before the interrupt,
//!   never a newer batch a late cancel response dispatched in the meantime.
//! - F4: a ping whose message is dropped (trigger already exists / hop limit /
//!   ping-pong damp) must not clear a user's forced chat sleep.

mod common;

use std::sync::Arc;

use chaos_core::{
    adapter::{AgentEvent, TurnOutcome},
    engine::{Effect, Engine, PostRequest},
    lifecycle::{LifecycleState, MembershipState},
    policy::Policy,
    store::Store,
    types::*,
};
use common::*;
use serde_json::json;

fn empty_script() -> chaos_core::fake::Script {
    script(json!({"turns": []}))
}

fn ping<'a>(author: MemberId, channel: ChannelId, body: &'a str) -> PostRequest<'a> {
    PostRequest {
        author,
        channel,
        body,
        reply_to: None,
        origin: Origin::Composer,
        client_request_id: None,
    }
}

fn dispatched(effects: Vec<Effect>) -> Vec<(MemberId, ChannelId, BatchId)> {
    effects
        .into_iter()
        .filter_map(|e| match e {
            Effect::Dispatch(d) => Some((d.agent, d.channel, d.batch)),
            _ => None,
        })
        .collect()
}

/// Fresh in-memory engine: one project, #main, human `hx`, agent `bot`
/// (globally Active). Returns `(engine, hx, bot, channel)`.
fn engine() -> (Engine, Member, Member, ChannelId) {
    let mut s = Store::open_in_memory().unwrap();
    let p = ProjectId::parse("0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59").unwrap();
    s.ensure_project(&p, "Test", None, 0).unwrap();
    let ch = s.ensure_channel(&p, "main", 0).unwrap();
    let hx = s.ensure_member(MemberKind::Human, "hx", "Hx").unwrap();
    let bot = s.ensure_member(MemberKind::Agent, "bot", "Bot").unwrap();
    s.join_project(&p, hx.id, 0).unwrap();
    s.join_project(&p, bot.id, 0).unwrap();
    s.join(ch.id, &hx, 20, 0).unwrap();
    s.join(ch.id, &bot, 20, 0).unwrap();
    s.set_agent_lifecycle(bot.id, LifecycleState::Active, false)
        .unwrap();
    let e = Engine::new(s, Policy::default(), 0).unwrap();
    (e, hx, bot, ch.id)
}

/// F1: work queued while active must not dispatch once the agent is
/// force-slept, even when something pumps; nothing opens a session while
/// asleep. Global sleep and per-chat sleep both gate dispatch.
#[test]
fn sleeping_agent_does_not_dispatch_queued_work() {
    let (mut e, hx, bot, ch) = engine();

    // Queue a ping without letting it dispatch, then force-sleep while queued.
    e.set_dispatch_hold(bot.id, ch, true);
    e.post(1, ping(hx.id, ch, "@bot hi")).unwrap();
    let _ = e.take_effects();
    assert!(e.store().pending_batch(bot.id, ch).unwrap().is_some());
    e.set_agent_lifecycle(bot.id, LifecycleState::Sleeping, true)
        .unwrap();
    e.set_dispatch_hold(bot.id, ch, false);

    // Pump (the tail of every control path) must be a no-op while asleep.
    e.settle_interrupted(2, bot.id, ch, Some(BatchId(999_999_999)));
    assert!(
        dispatched(e.take_effects()).is_empty(),
        "no dispatch while globally sleeping"
    );
    assert!(
        e.store().in_flight_batch(bot.id, ch).unwrap().is_none(),
        "no session opened while sleeping"
    );
    // Queued work is preserved, not dropped.
    assert!(e.store().pending_batch(bot.id, ch).unwrap().is_some());
}

/// F1: a per-chat sleeping membership blocks dispatch in that chat even when
/// the agent is globally Active.
#[test]
fn sleeping_chat_membership_does_not_dispatch_queued_work() {
    let (mut e, hx, bot, ch) = engine();

    e.set_dispatch_hold(bot.id, ch, true);
    e.post(1, ping(hx.id, ch, "@bot hi")).unwrap();
    let _ = e.take_effects();
    // Sleep the chat membership for every chat on this channel.
    let chat = e.store().chats().unwrap()[0].id;
    e.set_chat_membership_lifecycle(chat, bot.id, MembershipState::Sleeping, true)
        .unwrap();
    e.set_dispatch_hold(bot.id, ch, false);

    e.settle_interrupted(2, bot.id, ch, Some(BatchId(999_999_999)));
    assert!(dispatched(e.take_effects()).is_empty());
    assert!(e.store().in_flight_batch(bot.id, ch).unwrap().is_none());
    assert!(e.store().pending_batch(bot.id, ch).unwrap().is_some());
}

/// F1: activating re-pumps work queued while the agent was asleep.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waking_repumps_work_queued_while_asleep() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", empty_script()).await;
    let main = h.main;

    // Queue a ping while dispatch is held, then force-sleep; nothing runs.
    h.core
        .with_engine(move |e, now| {
            e.set_dispatch_hold(bot.id, main, true);
            let _ = e.post(
                now,
                PostRequest {
                    author: h.hx.id,
                    channel: main,
                    body: "@bot hi",
                    reply_to: None,
                    origin: Origin::Composer,
                    client_request_id: None,
                },
            );
            e.set_agent_lifecycle(bot.id, LifecycleState::Sleeping, true)
                .unwrap();
            e.set_dispatch_hold(bot.id, main, false);
        })
        .await
        .unwrap();
    h.quiesce().await;
    assert!(
        h.prompts("bot").is_empty(),
        "nothing dispatches while asleep"
    );

    // Global activation wakes + re-pumps the queued ping.
    h.core
        .set_agent_lifecycle(bot.id, LifecycleState::Active, false)
        .await
        .unwrap();
    h.quiesce().await;
    assert_eq!(h.prompts("bot").len(), 1, "activation re-pumps queued work");
}

/// F2: a runtime control settles the batch captured before the interrupt —
/// never a newer batch that a late cancel response pumped in meanwhile.
#[test]
fn settle_interrupted_settles_the_captured_batch_not_in_flight() {
    let (mut e, hx, bot, ch) = engine();

    // A starts; a second ping queues B behind it; then A's late TurnEnded
    // settles A and pumps B in flight.
    e.post(1, ping(hx.id, ch, "@bot first")).unwrap();
    let a = e
        .store()
        .in_flight_batch(bot.id, ch)
        .unwrap()
        .expect("A in flight");
    e.post(2, ping(hx.id, ch, "@bot second")).unwrap();
    let _ = e.take_effects();
    e.agent_event(
        3,
        bot.id,
        ch,
        AgentEvent::TurnEnded {
            turn: a.id,
            outcome: TurnOutcome::Completed,
            final_text: Arc::from(""),
        },
    );
    let _ = e.take_effects();
    let b = e
        .store()
        .in_flight_batch(bot.id, ch)
        .unwrap()
        .expect("B pumped in after A settled");
    assert_ne!(a.id, b.id);

    // The runtime control carries A (captured before the interrupt); the late
    // response already settled A. B must not be force-cancelled.
    e.settle_interrupted(4, bot.id, ch, Some(a.id));
    let _ = e.take_effects();
    let still = e
        .store()
        .in_flight_batch(bot.id, ch)
        .unwrap()
        .expect("B still in flight");
    assert_eq!(still.id, b.id, "the newer batch is not destroyed");
}

/// F2: nothing was in flight when the interrupt started → settle nothing,
/// even if something is in flight now.
#[test]
fn settle_interrupted_with_no_captured_batch_settles_nothing() {
    let (mut e, hx, bot, ch) = engine();
    e.post(1, ping(hx.id, ch, "@bot hi")).unwrap();
    let _ = e.take_effects();
    let in_flight = e.store().in_flight_batch(bot.id, ch).unwrap().unwrap();

    e.settle_interrupted(2, bot.id, ch, None);
    let _ = e.take_effects();
    assert_eq!(
        e.store().in_flight_batch(bot.id, ch).unwrap().unwrap().id,
        in_flight.id,
        "None captured batch settles nothing"
    );
}

/// F4: a ping whose trigger is dropped (already exists / hop limit / damp)
/// must not clear a user's forced chat sleep.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_ping_does_not_clear_forced_chat_sleep() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", empty_script()).await;
    let chat = h.core.chats().await.unwrap()[0].id;

    // A ping that does create a trigger (and wakes a sleeping membership).
    let posted = h.core.post(h.hx.id, h.main, "@bot hi", None).await.unwrap();
    h.quiesce().await;

    // User force-sleeps the chat membership afterwards.
    h.core
        .set_chat_membership_lifecycle(chat, bot.id, MembershipState::Sleeping, true)
        .await
        .unwrap();

    // Retriggering the same message pings the agent but is dropped because
    // the trigger already exists; the forced sleep must survive.
    h.core.retrigger(posted.id).await.unwrap();
    h.quiesce().await;

    assert!(
        h.core
            .chat_membership_forced_sleep(chat, bot.id)
            .await
            .unwrap(),
        "a dropped ping must not clear forced chat sleep"
    );
    assert_eq!(
        h.core
            .chat_membership_lifecycle(chat, bot.id)
            .await
            .unwrap(),
        MembershipState::Sleeping
    );
}
