//! Model members (a model running through a harness): model selection over
//! ACP config options, model discovery, the native Codex app-server adapter
//! (against the in-process fake app-server) and the per-project roster.

mod common;

use std::{sync::Arc, time::Duration};

use chaos_core::{
    acp::{AcpAdapter, FnTransport},
    adapter::AgentAdapter,
    codex::CodexAdapter,
    engine::CoreEvent,
    fake::{self, FakeAgent},
    fake_codex::{self, FakeCodex, Step},
    host::MemberSpec,
    permission::{PermissionState, RunsIn},
    roster::ModelMember,
    types::*,
};
use common::*;
use serde_json::json;

fn acp_adapter(fake: &FakeAgent, model: Option<&str>) -> Arc<AcpAdapter> {
    let f = fake.clone();
    Arc::new(
        AcpAdapter::new(Arc::new(FnTransport("fake acp".into(), move || {
            fake::in_process(f.clone())
        })))
        .prefer_v2(false)
        .model(model.map(Arc::from)),
    )
}

async fn add(
    h: &Harness,
    harness: &str,
    model: Option<&str>,
    name: &str,
    adapter: Arc<dyn AgentAdapter>,
    runs_in: RunsIn,
) -> ModelMember {
    h.core
        .add_model_member(
            MemberSpec {
                harness: harness.into(),
                model: model.map(str::to_string),
                model_name: name.into(),
                adapter,
                cwd: Some(std::env::temp_dir()),
                runs_in,
            },
            h.main,
        )
        .await
        .unwrap()
}

/// Polls `f` (real time, bounded) until it returns `Some`.
async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn models_script() -> serde_json::Value {
    json!({
        "models": [["fast-1", "Fast One"], ["big-2", "Big Two"]],
        "turns": [{"steps": [{"text": "On it"}]}]
    })
}

// ---------- ACP ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_member_runs_its_model_through_the_session_model_option() {
    let mut h = Harness::new().await;
    let fake = FakeAgent::new(script(models_script()));
    let m = add(
        &h,
        "fx",
        Some("big-2"),
        "Big Two",
        acp_adapter(&fake, Some("big-2")),
        RunsIn::Host,
    )
    .await;
    assert_eq!(&*m.member.handle, "big-two");
    h.say("@big-two hello").await;
    h.wait_message("the fallback reply", Some(m.member.id))
        .await;
    let o = fake.observed.lock().unwrap().clone();
    assert_eq!(
        o.config_sets,
        vec![("model".to_string(), "big-2".to_string())]
    );
    // The session reported the harness's models; they are cached for the
    // picker and announced.
    match h
        .wait_for("models event", |e| matches!(e, CoreEvent::Models { .. }))
        .await
    {
        CoreEvent::Models { harness, models } => {
            assert_eq!(&*harness, "fx");
            assert_eq!(models.len(), 2);
        }
        _ => unreachable!(),
    }
    let cached = h.core.harness_models("fx").await.unwrap();
    assert_eq!(cached[1].id.as_deref(), Some("big-2"));
    assert_eq!(&*cached[1].name, "Big Two");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_the_harness_does_not_offer_runs_the_default_and_says_so() {
    let mut h = Harness::new().await;
    let fake = FakeAgent::new(script(models_script()));
    let m = add(
        &h,
        "fx",
        Some("gone-9"),
        "Gone Nine",
        acp_adapter(&fake, Some("gone-9")),
        RunsIn::Host,
    )
    .await;
    h.say("@gone-nine hello").await;
    h.wait_message("the reply", Some(m.member.id)).await;
    assert!(fake.observed.lock().unwrap().config_sets.is_empty());
    let presence = eventually("a model note", async || {
        let s = h.core.status(m.member.id, h.main).await.ok()?;
        s.presence.filter(|p| p.contains("gone-9"))
    })
    .await;
    assert!(presence.contains("running its default"), "{presence}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_model_member_sets_nothing() {
    let mut h = Harness::new().await;
    let fake = FakeAgent::new(script(models_script()));
    let m = add(
        &h,
        "fx",
        None,
        "fx default",
        acp_adapter(&fake, None),
        RunsIn::Host,
    )
    .await;
    assert_eq!(&*m.member.handle, "fx");
    h.say("@fx hello").await;
    h.wait_message("the reply", Some(m.member.id)).await;
    assert!(fake.observed.lock().unwrap().config_sets.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_discovery_opens_and_closes_a_throwaway_session_without_chat_tools() {
    let h = Harness::new().await;
    let fake = FakeAgent::new(script(models_script()));
    let found = h
        .core
        .discover_models("fx", acp_adapter(&fake, None), std::env::temp_dir())
        .await
        .unwrap();
    assert_eq!(
        found
            .iter()
            .map(|m| m.id.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["fast-1", "big-2"]
    );
    let o = fake.observed.lock().unwrap().clone();
    assert_eq!(o.sessions.len(), 1);
    assert_eq!(o.sessions[0]["mcpServers"], json!([]), "no chat tools");
    assert_eq!(o.closed.len(), 1, "the session is closed again");
    assert!(o.prompts.is_empty());
    assert_eq!(h.core.harness_models("fx").await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_of_a_signed_out_harness_is_an_error_and_caches_nothing() {
    let h = Harness::new().await;
    let fake = FakeAgent::new(script(json!({
        "fail_session_new": "Authentication required", "models": [["a", "A"]]
    })));
    let err = h
        .core
        .discover_models("fx", acp_adapter(&fake, None), std::env::temp_dir())
        .await
        .unwrap_err();
    assert!(err.contains("Authentication required"), "{err}");
    assert!(h.core.harness_models("fx").await.unwrap().is_empty());
}

// ---------- Codex app-server ----------

fn codex(script: fake_codex::Script, model: Option<&str>) -> (FakeCodex, Arc<CodexAdapter>) {
    let fake = FakeCodex::new(script);
    let adapter = Arc::new(CodexAdapter::new(Arc::new(fake.clone())).model(model.map(Arc::from)));
    (fake, adapter)
}

fn sol_models() -> Vec<(String, String)> {
    vec![
        ("gpt-6.1-sol".into(), "GPT-6.1 Sol".into()),
        ("gpt-6-luna".into(), "GPT-6 Luna".into()),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_member_takes_a_turn_and_its_last_message_is_the_fallback_reply() {
    let mut h = Harness::new().await;
    let (fake, adapter) = codex(
        fake_codex::Script {
            models: sol_models(),
            turns: vec![fake_codex::Turn {
                steps: vec![
                    Step::Message("Let me check".into()),
                    Step::Command("cargo test".into()),
                    Step::Message("All green.".into()),
                ],
                ..Default::default()
            }],
            ..Default::default()
        },
        Some("gpt-6.1-sol"),
    );
    let m = add(
        &h,
        "codex",
        Some("gpt-6.1-sol"),
        "GPT-6.1 Sol",
        adapter,
        RunsIn::Host,
    )
    .await;
    let ping = h.say("@gpt-6-1-sol run the tests").await;
    let reply = h
        .wait_message("the fallback reply", Some(m.member.id))
        .await;
    assert_eq!(&*reply.body, "All green.");
    assert_eq!(reply.reply_to, Some(ping.id));

    assert_eq!(
        fake.methods()[..4],
        ["initialize", "initialized", "model/list", "thread/start"]
    );
    let start = fake.request("thread/start").unwrap();
    assert_eq!(start["model"], "gpt-6.1-sol");
    // Reads never prompt; anything more asks (and the gate declines it).
    assert_eq!(start["sandbox"], "read-only");
    assert_eq!(start["approvalPolicy"], "on-request");
    let mcp = &start["config"]["mcp_servers.chaos-control"];
    assert!(
        mcp["url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    assert!(
        mcp["http_headers"]["Authorization"]
            .as_str()
            .unwrap()
            .starts_with("Bearer ")
    );
    assert!(start.get("approvalPolicy").is_none(), "never overridden");
    let turn = fake.request("turn/start").unwrap();
    assert_eq!(turn["threadId"], "thr_1");
    assert!(
        turn["input"][0]["text"]
            .as_str()
            .unwrap()
            .contains("run the tests")
    );
    // model/list fed the picker cache.
    assert_eq!(h.core.harness_models("codex").await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_model_missing_from_model_list_starts_on_the_default() {
    let mut h = Harness::new().await;
    let (fake, adapter) = codex(
        fake_codex::Script {
            models: sol_models(),
            turns: vec![fake_codex::Turn {
                steps: vec![Step::Message("ok".into())],
                ..Default::default()
            }],
            ..Default::default()
        },
        Some("gpt-old"),
    );
    let m = add(
        &h,
        "codex",
        Some("gpt-old"),
        "GPT Old",
        adapter,
        RunsIn::Host,
    )
    .await;
    h.say("@gpt-old hi").await;
    h.wait_message("the reply", Some(m.member.id)).await;
    assert!(fake.request("thread/start").unwrap().get("model").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_approvals_go_through_the_permission_gate() {
    // On the host: declined natively (the turn carries on), sent to the humans.
    let mut h = Harness::new().await;
    let (fake, adapter) = codex(
        fake_codex::Script {
            turns: vec![fake_codex::Turn {
                steps: vec![
                    Step::Approval("rm -rf build/".into()),
                    Step::Message("Asked; doing the docs meanwhile.".into()),
                ],
                ..Default::default()
            }],
            ..Default::default()
        },
        None,
    );
    let m = add(&h, "codex", None, "Codex default", adapter, RunsIn::Host).await;
    h.say("@codex clean up").await;
    let req = match h
        .wait_for(
            "a pending request",
            |e| matches!(e, CoreEvent::Permission(r) if r.state == PermissionState::Pending),
        )
        .await
    {
        CoreEvent::Permission(r) => r,
        _ => unreachable!(),
    };
    assert_eq!(&*req.title, "rm -rf build/");
    assert_eq!(req.agent, m.member.id);
    h.wait_message("the turn carried on", Some(m.member.id))
        .await;
    assert_eq!(*fake.decisions.lock().unwrap(), ["decline"]);

    // In its own VM: accepted.
    let mut h = Harness::new().await;
    let (fake, adapter) = codex(
        fake_codex::Script {
            turns: vec![fake_codex::Turn {
                steps: vec![
                    Step::Approval("make".into()),
                    Step::Message("Built.".into()),
                ],
                ..Default::default()
            }],
            ..Default::default()
        },
        None,
    );
    let m = add(&h, "codex", None, "Codex default", adapter, RunsIn::Vm).await;
    h.say("@codex build it").await;
    h.wait_message("the reply", Some(m.member.id)).await;
    assert_eq!(*fake.decisions.lock().unwrap(), ["accept"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_signed_out_goes_red_without_retrying() {
    let h = Harness::new().await;
    let (fake, adapter) = codex(
        fake_codex::Script {
            thread_error: Some("Not logged in. Run `codex login`.".into()),
            ..Default::default()
        },
        None,
    );
    let m = add(&h, "codex", None, "Codex default", adapter, RunsIn::Host).await;
    h.say("@codex hi").await;
    let status = eventually("red", async || {
        let s = h.core.status(m.member.id, h.main).await.ok()?;
        (s.glow == Glow::Unresponsive).then_some(s)
    })
    .await;
    assert!(status.presence.unwrap_or_default().contains("logged in"));
    h.advance(60_000).await;
    h.quiesce().await;
    let starts = fake
        .methods()
        .iter()
        .filter(|m| *m == "thread/start")
        .count();
    assert_eq!(starts, 1, "an auth failure is never retried");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_interrupt_ends_the_turn_cancelled_with_no_fallback() {
    let mut h = Harness::new().await;
    let (fake, adapter) = codex(
        fake_codex::Script {
            turns: vec![fake_codex::Turn {
                steps: vec![
                    Step::Message("Starting".into()),
                    Step::Sleep(10_000),
                    Step::Message("never".into()),
                ],
                ..Default::default()
            }],
            ..Default::default()
        },
        None,
    );
    let m = add(&h, "codex", None, "Codex default", adapter, RunsIn::Host).await;
    let ping = h.say("@codex long job").await;
    eventually("turn started", async || {
        fake.methods()
            .iter()
            .any(|m| m == "turn/start")
            .then_some(())
    })
    .await;
    // Let the turn be accepted before cancelling.
    eventually("accepted", async || {
        let s = h.core.status(m.member.id, h.main).await.ok()?;
        (s.glow == Glow::Active).then_some(())
    })
    .await;
    h.core.interrupt(m.member.id, h.main);
    eventually("interrupt sent", async || {
        fake.methods()
            .iter()
            .any(|m| m == "turn/interrupt")
            .then_some(())
    })
    .await;
    let state = eventually("obligation unmet", async || {
        let s = h.obligation(m.member.id, ping.id).await?;
        (s != ObligationState::Open).then_some(s)
    })
    .await;
    assert_eq!(state, ObligationState::Unmet);
    h.quiesce().await;
    assert!(
        h.by(m.member.id).await.is_empty(),
        "no fallback after a cancel"
    );
    let _ = &mut h;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_lists_models_without_starting_a_thread() {
    let (fake, adapter) = codex(
        fake_codex::Script {
            models: sol_models(),
            ..Default::default()
        },
        None,
    );
    let found = adapter
        .list_models(std::env::temp_dir())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(&*found[0].name, "GPT-6.1 Sol");
    assert!(!fake.methods().iter().any(|m| m == "thread/start"));
}

// ---------- roster ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_harness_hosts_two_members_and_removing_one_leaves_the_other() {
    let mut h = Harness::new().await;
    let fake_a = FakeAgent::new(script(models_script()));
    let fake_b = FakeAgent::new(script(models_script()));
    let a = add(
        &h,
        "fx",
        Some("fast-1"),
        "Fast One",
        acp_adapter(&fake_a, Some("fast-1")),
        RunsIn::Host,
    )
    .await;
    let b = add(
        &h,
        "fx",
        Some("big-2"),
        "Big Two",
        acp_adapter(&fake_b, Some("big-2")),
        RunsIn::Host,
    )
    .await;
    assert_ne!(a.member.id, b.member.id);
    let project = common::project_id();
    let roster = h.core.roster(&project).await.unwrap();
    assert_eq!(
        roster.iter().map(|m| &*m.member.handle).collect::<Vec<_>>(),
        ["fast-one", "big-two"]
    );

    // Adding again is the same member (startup re-registration).
    let again = add(
        &h,
        "fx",
        Some("big-2"),
        "Big Two",
        acp_adapter(&fake_b, Some("big-2")),
        RunsIn::Host,
    )
    .await;
    assert_eq!(again.member.id, b.member.id);
    assert_eq!(h.core.roster(&project).await.unwrap().len(), 2);

    h.say("@fast-one hi").await;
    h.wait_message("a's reply", Some(a.member.id)).await;

    h.core.remove_member(&project, a.member.id).await.unwrap();
    let roster = h.core.roster(&project).await.unwrap();
    assert_eq!(roster, vec![b.clone()]);
    let members = h.core.members(h.main).await.unwrap();
    assert!(!members.iter().any(|m| m.id == a.member.id));
    // Its history stays.
    assert_eq!(h.by(a.member.id).await.len(), 1);

    // A mention of the removed member wakes nobody; the other still answers.
    let prompts_before = fake_a.observed.lock().unwrap().prompts.len();
    h.say("@fast-one @big-two both of you").await;
    h.wait_message("b's reply", Some(b.member.id)).await;
    h.quiesce().await;
    assert_eq!(
        fake_a.observed.lock().unwrap().prompts.len(),
        prompts_before
    );

    // Seeding happens once per project.
    assert!(h.core.seed_roster_once(&project).await.unwrap());
    assert!(!h.core.seed_roster_once(&project).await.unwrap());
}

// ---------- discovery off the runtime (the "Add a model" crash) ----------

/// The app awaits core's futures on GPUI's executor, which is not a tokio
/// runtime. Discovery used to run the adapter's I/O (process spawn,
/// `tokio::spawn`, timers) inside the caller's future, which panics off a
/// tokio runtime ("there is no reactor running"): clicking "Add a model"
/// crashed the app. It now runs on core's own runtime, so any executor can
/// await it.
#[test]
fn discovery_can_be_awaited_from_outside_the_tokio_runtime() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(Harness::new());
    let (_fake, codex_adapter) = codex(
        fake_codex::Script {
            models: sol_models(),
            ..Default::default()
        },
        None,
    );
    let fake = FakeAgent::new(script(models_script()));
    // Not inside `rt`: like GPUI's foreground executor.
    let codex_found = futures::executor::block_on(h.core.discover_models(
        "codex",
        codex_adapter,
        std::env::temp_dir(),
    ))
    .unwrap();
    assert_eq!(codex_found.len(), 2);
    let acp_found = futures::executor::block_on(h.core.discover_models(
        "fx",
        acp_adapter(&fake, None),
        std::env::temp_dir(),
    ))
    .unwrap();
    assert_eq!(acp_found.len(), 2);
    drop(h);
    drop(rt);
}
