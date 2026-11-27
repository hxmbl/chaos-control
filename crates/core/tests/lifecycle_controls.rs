//! STRM-687: scoped Stop / Force sleep with observable interruption outcomes.
//!
//! Stop interrupts without changing wake preference; Force sleep interrupts
//! first and then persists Sleeping at the selected scope. The report says
//! per channel whether cancellation was confirmed, unsupported, failed, or
//! fell back to closing the owned session.

mod common;

use std::{path::PathBuf, sync::Arc, time::Duration};

use chaos_core::{
    acp::{AcpAdapter, FnTransport},
    adapter::{
        AgentAdapter, AgentError, AgentErrorKind, AgentSession, Caps, OpenSession, Probe,
        SessionInfo,
    },
    fake::{self, FakeAgent, Script},
    host::{AgentSpec, Core, HostConfig, ManualClock},
    lifecycle::{InterruptOutcome, LifecycleScope, LifecycleState, MembershipState, SleepApplied},
    types::*,
};
use common::*;
use futures::future::BoxFuture;
use serde_json::json;

fn slow_turn() -> Script {
    script(json!({"turns": [{"steps": [{"gate": "never"}]}]}))
}

fn cancels(h: &Harness, handle: &str) -> usize {
    h.fake(handle).observed.lock().unwrap().cancels
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_confirms_interruption_without_changing_lifecycle() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", slow_turn()).await;
    h.say("@bot work on it").await;
    h.wait_prompts("bot", 1).await;

    let outcome = h.core.stop(bot.id, LifecycleScope::Global).await.unwrap();
    assert_eq!(outcome.sleep, None, "Stop never writes lifecycle state");
    assert!(
        outcome.all_interrupted(),
        "the ACP session acknowledged the cancel"
    );
    assert_eq!(outcome.interruptions.len(), 1);
    assert_eq!(outcome.interruptions[0].0, h.main);
    assert_eq!(outcome.interruptions[0].1, InterruptOutcome::Confirmed);
    wait_cancels(&h, "bot", 1).await;
    assert_eq!(
        h.core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Active,
        "wake preference is unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_with_no_session_reports_no_active_turn() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", slow_turn()).await;
    // No message yet, so there is no live session to interrupt.
    let outcome = h.core.stop(bot.id, LifecycleScope::Global).await.unwrap();
    assert_eq!(
        outcome.interruptions,
        vec![(h.main, InterruptOutcome::NoActiveTurn)]
    );
    assert!(
        outcome.all_interrupted(),
        "nothing to cancel is not a failure"
    );
    assert_eq!(cancels(&h, "bot"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_sleep_at_chat_scope_only_sleeps_that_chat() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", slow_turn()).await;
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

    // One slow turn in each chat, so both sessions are live.
    h.say("@bot main work").await;
    h.core
        .post(h.hx.id, side.id, "@bot side work", None)
        .await
        .unwrap();
    h.wait_prompts("bot", 2).await;

    let outcome = h
        .core
        .force_sleep(bot.id, LifecycleScope::Chat(side_chat.id))
        .await
        .unwrap();
    assert_eq!(outcome.interruptions.len(), 1, "only the chat's target");
    assert_eq!(outcome.interruptions[0].0, side.id);
    assert_eq!(outcome.interruptions[0].1, InterruptOutcome::Confirmed);
    assert_eq!(
        outcome.sleep,
        Some(SleepApplied::Chat(side_chat.id, MembershipState::Sleeping))
    );
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
    assert_eq!(
        h.core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Active,
        "global state is untouched"
    );
    wait_cancels(&h, "bot", 1).await;
    assert_eq!(cancels(&h, "bot"), 1, "the other chat was not interrupted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_sleep_globally_interrupts_every_chat() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", slow_turn()).await;
    let side = h.core.channel(&project_id(), "side").await.unwrap();
    h.core.join(h.hx.id, side.id).await.unwrap();
    h.core.join(bot.id, side.id).await.unwrap();
    h.say("@bot main work").await;
    h.core
        .post(h.hx.id, side.id, "@bot side work", None)
        .await
        .unwrap();
    h.wait_prompts("bot", 2).await;

    let outcome = h
        .core
        .force_sleep(bot.id, LifecycleScope::Global)
        .await
        .unwrap();
    assert_eq!(outcome.interruptions.len(), 2, "every joined channel");
    assert!(
        outcome
            .interruptions
            .iter()
            .all(|(_, o)| *o == InterruptOutcome::Confirmed)
    );
    assert_eq!(
        outcome.sleep,
        Some(SleepApplied::Global(LifecycleState::Sleeping))
    );
    assert_eq!(
        h.core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Sleeping
    );
    assert!(h.core.agent_forced_sleep(bot.id).await.unwrap());
    wait_cancels(&h, "bot", 2).await;
}

/// An adapter whose sessions do not advertise `INTERRUPT`. We wrap the real
/// ACP adapter so everything else (prompts, close, events) is unchanged.
struct NoInterruptAdapter(Arc<AcpAdapter>);

impl AgentAdapter for NoInterruptAdapter {
    fn describe(&self) -> String {
        self.0.describe()
    }
    fn probe(&self) -> BoxFuture<'_, Probe> {
        self.0.probe()
    }
    fn open(&self, req: OpenSession) -> BoxFuture<'_, Result<Arc<dyn AgentSession>, AgentError>> {
        let inner = self.0.clone();
        Box::pin(async move {
            let session = inner.open(req).await?;
            let mut info = session.info().clone();
            info.caps = Caps::empty();
            Ok(Arc::new(NoInterruptSession {
                inner: session,
                info,
            }) as Arc<dyn AgentSession>)
        })
    }
}

struct NoInterruptSession {
    inner: Arc<dyn AgentSession>,
    info: SessionInfo,
}

impl AgentSession for NoInterruptSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }
    fn deliver(&self, turn: BatchId, prompt: Arc<str>) -> BoxFuture<'_, Result<(), AgentError>> {
        self.inner.deliver(turn, prompt)
    }
    fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        panic!("a session without INTERRUPT must never be asked to cancel")
    }
    fn close(&self) -> BoxFuture<'_, ()> {
        self.inner.close()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_interrupt_is_reported_and_the_session_is_closed() {
    let clock = ManualClock::new(T0);
    let (core, _events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();

    let fake = FakeAgent::new(slow_turn());
    let f = fake.clone();
    let acp = Arc::new(AcpAdapter::new(Arc::new(FnTransport(
        "fake:nointerrupt".into(),
        move || fake::in_process(f.clone()),
    ))));
    let bot = core
        .add_agent(
            AgentSpec {
                handle: "nointerrupt".into(),
                name: "NoInterrupt".into(),
                adapter: Arc::new(NoInterruptAdapter(acp)),
                cwd: Some(PathBuf::from("/tmp")),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    core.set_agent_lifecycle(bot.id, LifecycleState::Active, false)
        .await
        .unwrap();
    core.post(hx.id, main, "@nointerrupt work", None)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while fake.observed.lock().unwrap().prompts.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session never opened"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let outcome = core.stop(bot.id, LifecycleScope::Global).await.unwrap();
    assert_eq!(outcome.interruptions.len(), 1);
    assert_eq!(outcome.interruptions[0].0, main);
    let InterruptOutcome::FallbackClosed { reason } = &outcome.interruptions[0].1 else {
        panic!(
            "expected a fallback close, got {:?}",
            outcome.interruptions[0].1
        );
    };
    assert!(reason.contains("capabilities"), "reason: {reason}");
    assert!(
        !outcome.all_interrupted(),
        "a fallback close is never a confirmed cancel"
    );
    assert_eq!(
        core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Active,
        "Stop leaves lifecycle alone"
    );
    assert_eq!(
        fake.observed.lock().unwrap().cancels,
        0,
        "no cancel was sent"
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_messages_do_not_requeue_a_sleeping_membership() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", slow_turn()).await;
    let chat = h.core.chats().await.unwrap()[0].id;
    h.core
        .force_sleep(bot.id, LifecycleScope::Chat(chat))
        .await
        .unwrap();

    h.say("ordinary chatter, no mention").await;
    h.say("@bot this is an explicit ping").await;
    h.quiesce().await;
    // The explicit ping wakes the membership once, then the next broadcast
    // flows normally. Sleeping itself never queued anything.
    assert_eq!(h.prompts("bot").len(), 1);
    assert_eq!(
        h.core
            .chat_membership_lifecycle(chat, bot.id)
            .await
            .unwrap(),
        MembershipState::Active
    );
}

/// Wraps a real ACP session but makes `interrupt()` fail. Used to cover
/// cancellation-failure reporting (STRM-687 gap).
struct FailInterruptAdapter(Arc<AcpAdapter>);

impl AgentAdapter for FailInterruptAdapter {
    fn describe(&self) -> String {
        self.0.describe()
    }
    fn probe(&self) -> BoxFuture<'_, Probe> {
        self.0.probe()
    }
    fn open(&self, req: OpenSession) -> BoxFuture<'_, Result<Arc<dyn AgentSession>, AgentError>> {
        let inner = self.0.clone();
        Box::pin(async move {
            let session = inner.open(req).await?;
            let info = session.info().clone();
            Ok(Arc::new(FailInterruptSession {
                inner: session,
                info,
            }) as Arc<dyn AgentSession>)
        })
    }
}

struct FailInterruptSession {
    inner: Arc<dyn AgentSession>,
    info: SessionInfo,
}

impl AgentSession for FailInterruptSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }
    fn deliver(&self, turn: BatchId, prompt: Arc<str>) -> BoxFuture<'_, Result<(), AgentError>> {
        self.inner.deliver(turn, prompt)
    }
    fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            Err(AgentError::new(
                AgentErrorKind::Protocol,
                "simulated interrupt failure",
            ))
        })
    }
    fn close(&self) -> BoxFuture<'_, ()> {
        self.inner.close()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_failure_with_fallback_reports_fallback_close() {
    let clock = ManualClock::new(T0);
    let (core, _events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();

    let fake = FakeAgent::new(slow_turn());
    let f = fake.clone();
    let acp = Arc::new(AcpAdapter::new(Arc::new(FnTransport(
        "fake:failinterrupt".into(),
        move || fake::in_process(f.clone()),
    ))));
    let bot = core
        .add_agent(
            AgentSpec {
                handle: "failinterrupt".into(),
                name: "FailInterrupt".into(),
                adapter: Arc::new(FailInterruptAdapter(acp)),
                cwd: Some(PathBuf::from("/tmp")),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    core.set_agent_lifecycle(bot.id, LifecycleState::Active, false)
        .await
        .unwrap();
    core.post(hx.id, main, "@failinterrupt work", None)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while fake.observed.lock().unwrap().prompts.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session never opened"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let outcome = core.stop(bot.id, LifecycleScope::Global).await.unwrap();
    assert_eq!(outcome.interruptions.len(), 1);
    assert_eq!(outcome.interruptions[0].0, main);
    let InterruptOutcome::FallbackClosed { reason } = &outcome.interruptions[0].1 else {
        panic!(
            "expected fallback close, got {:?}",
            outcome.interruptions[0].1
        );
    };
    assert!(reason.contains("interrupt failed"), "reason: {reason}");
    assert!(
        reason.contains("simulated interrupt failure"),
        "reason: {reason}"
    );
    assert!(!outcome.all_interrupted(), "fallback is not confirmed");
    assert_eq!(
        fake.observed.lock().unwrap().cancels,
        0,
        "no successful cancel was recorded"
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_failure_without_fallback_reports_failed() {
    let clock = ManualClock::new(T0);
    let (core, _events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();

    let fake = FakeAgent::new(slow_turn());
    let f = fake.clone();
    let acp = Arc::new(AcpAdapter::new(Arc::new(FnTransport(
        "fake:failnofallback".into(),
        move || fake::in_process(f.clone()),
    ))));
    let bot = core
        .add_agent(
            AgentSpec {
                handle: "failnofallback".into(),
                name: "FailNoFallback".into(),
                adapter: Arc::new(FailInterruptAdapter(acp)),
                cwd: Some(PathBuf::from("/tmp")),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    core.set_agent_lifecycle(bot.id, LifecycleState::Active, false)
        .await
        .unwrap();
    core.post(hx.id, main, "@failnofallback work", None)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while fake.observed.lock().unwrap().prompts.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session never opened"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // stop_and_check_chat uses interrupt_scoped(..., fallback_close=false).
    let outcome = core
        .stop_and_check_chat(bot.id, LifecycleScope::Global)
        .await
        .unwrap();
    assert_eq!(outcome.interruptions.len(), 1);
    assert_eq!(outcome.interruptions[0].0, main);
    match &outcome.interruptions[0].1 {
        InterruptOutcome::Failed(msg) => {
            assert!(msg.contains("simulated interrupt failure"), "msg: {msg}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(
        outcome.instructed.is_empty(),
        "failed interrupt must not deliver a context-refresh instruction"
    );
    assert!(
        outcome.no_session.is_empty(),
        "session existed; failure is not a missing session"
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_chat_scope_does_not_interrupt_other_chat() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", slow_turn()).await;
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
        .stop(bot.id, LifecycleScope::Chat(side_chat.id))
        .await
        .unwrap();
    assert_eq!(outcome.sleep, None);
    assert_eq!(outcome.interruptions.len(), 1);
    assert_eq!(outcome.interruptions[0].0, side.id);
    assert_eq!(outcome.interruptions[0].1, InterruptOutcome::Confirmed);
    wait_cancels(&h, "bot", 1).await;
    assert_eq!(cancels(&h, "bot"), 1, "main chat must not be interrupted");
}

/// STRM-687 reconnect: a session that had a live integration but cannot be
/// interrupted is closed (`FallbackClosed`); when the agent reconnects on a
/// later turn the fresh session starts clean - no re-sent cancel, no requeued
/// pre-stop work, lifecycle scope preserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_after_fallback_close_starts_clean_without_requeue() {
    let clock = ManualClock::new(T0);
    let (core, _events) = Core::start(HostConfig::default(), clock.clone())
        .await
        .unwrap();
    let main = main_channel(&core).await.id;
    let hx = core.human("hx", "Hx", main).await.unwrap();

    // A quick turn: the first session finishes before we close it, so
    // reconnect behaviour is isolated from cancellation-failure semantics.
    let fake = FakeAgent::new(script(json!({"turns": [{"steps": []}]})));
    let f = fake.clone();
    let acp = Arc::new(AcpAdapter::new(Arc::new(FnTransport(
        "fake:nointerrupt".into(),
        move || fake::in_process(f.clone()),
    ))));
    let bot = core
        .add_agent(
            AgentSpec {
                handle: "nointerrupt".into(),
                name: "NoInterrupt".into(),
                adapter: Arc::new(NoInterruptAdapter(acp)),
                cwd: Some(PathBuf::from("/tmp")),
                ping_answers: None,
            },
            main,
        )
        .await
        .unwrap();
    core.set_agent_lifecycle(bot.id, LifecycleState::Active, false)
        .await
        .unwrap();

    // First turn on the original session, allowed to finish.
    core.post(hx.id, main, "@nointerrupt work", None)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while fake.observed.lock().unwrap().prompts.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session never opened"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let prompts_before = fake.observed.lock().unwrap().prompts.len();
    let sessions_before = fake.observed.lock().unwrap().sessions.len();

    // Unsupported interrupt -> owned session closed as a fallback. No turn is
    // in flight, so nothing is retried or requeued.
    let outcome = core.stop(bot.id, LifecycleScope::Global).await.unwrap();
    assert!(matches!(
        outcome.interruptions[0].1,
        InterruptOutcome::FallbackClosed { .. }
    ));
    // The owned session is closed best-effort; give the close event a moment
    // to drop the slot before reconnecting.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        fake.observed.lock().unwrap().cancels,
        0,
        "no cancel was sent"
    );
    assert_eq!(
        core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Active,
        "Stop leaves lifecycle alone across reconnect"
    );

    // Reconnect: a later turn opens a fresh session.
    core.post(hx.id, main, "again", None).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (prompts, sessions) = {
            let o = fake.observed.lock().unwrap();
            (o.prompts.len(), o.sessions.len())
        };
        if prompts > prompts_before {
            // The fresh session received exactly the new prompt; the
            // pre-stop work was not requeued.
            assert_eq!(prompts, prompts_before + 1, "no requeued pre-stop work");
            assert!(
                sessions > sessions_before,
                "reconnect opened a fresh session: {sessions} vs {sessions_before}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "reconnect never redelivered"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        fake.observed.lock().unwrap().cancels,
        0,
        "no cancel on reconnect"
    );
    assert_eq!(
        core.agent_lifecycle(bot.id).await.unwrap(),
        LifecycleState::Active,
        "lifecycle state survives reconnect"
    );
    core.shutdown().await;
}
