//! Model members (a model running through a harness): model selection over
//! ACP config options, model discovery and the per-project roster.

mod common;

use std::{sync::Arc, time::Duration};

use chaos_core::{
    acp::{AcpAdapter, FnTransport},
    adapter::AgentAdapter,
    engine::CoreEvent,
    fake::{self, FakeAgent},
    host::MemberSpec,
    permission::RunsIn,
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
                approval: ApprovalMode::AskAll,
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

/// Devin refuses untrusted folders, so the ACP adapter adds the folder to
/// Devin's trusted workspaces before model discovery and before a session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn devin_folders_are_trusted_before_discovery_and_sessions() {
    let mut h = Harness::new().await;
    let root = std::env::temp_dir().join(format!("chaos-devin-trust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (probe_dir, work_dir) = (root.join("probe"), root.join("work"));
    std::fs::create_dir_all(&probe_dir).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();
    let trust = root.join("data/devin/cli/trusted_workspaces.json");
    let fake = FakeAgent::new(script(models_script()));
    let f = fake.clone();
    let adapter = Arc::new(
        AcpAdapter::new(Arc::new(FnTransport("fake devin".into(), move || {
            fake::in_process(f.clone())
        })))
        .prefer_v2(false)
        .devin_trust_file(trust.clone()),
    );
    let trusted = || -> Vec<String> {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&trust).unwrap()).unwrap();
        v["trusted_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect()
    };
    let canon = |p: &std::path::Path| {
        std::fs::canonicalize(p)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };

    h.core
        .discover_models("devin", adapter.clone(), probe_dir.clone())
        .await
        .unwrap();
    assert_eq!(trusted(), vec![canon(&probe_dir)]);

    let m = h
        .core
        .add_model_member(
            MemberSpec {
                harness: "devin".into(),
                model: None,
                model_name: "Devin".into(),
                adapter,
                cwd: Some(work_dir.clone()),
                runs_in: RunsIn::Host,
                approval: ApprovalMode::AskAll,
            },
            h.main,
        )
        .await
        .unwrap();
    h.say(&format!("@{} hello", m.member.handle)).await;
    h.wait_message("the reply", Some(m.member.id)).await;
    assert_eq!(trusted(), vec![canon(&probe_dir), canon(&work_dir)]);
    let _ = std::fs::remove_dir_all(&root);
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
    // Its history stays, and core still knows it was an agent (the app
    // labels those messages "Previous agent").
    assert_eq!(h.by(a.member.id).await.len(), 1);
    let agents = h.core.agent_members().await.unwrap();
    assert!(agents.contains(&a.member.id) && agents.contains(&b.member.id));
    assert!(!agents.contains(&h.hx.id));

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
    let fake_a = FakeAgent::new(script(models_script()));
    let fake_b = FakeAgent::new(script(models_script()));
    // Not inside `rt`: like GPUI's foreground executor.
    let found_a = futures::executor::block_on(h.core.discover_models(
        "fx",
        acp_adapter(&fake_a, None),
        std::env::temp_dir(),
    ))
    .unwrap();
    assert_eq!(found_a.len(), 2);
    let found_b = futures::executor::block_on(h.core.discover_models(
        "vibe",
        acp_adapter(&fake_b, None),
        std::env::temp_dir(),
    ))
    .unwrap();
    assert_eq!(found_b.len(), 2);
    drop(h);
    drop(rt);
}

/// The background probe writes the harness's list to the on-disk cache
/// under its version, which is what the picker reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_probe_writes_the_model_cache_for_the_picker() {
    let h = Harness::new().await;
    let fake = FakeAgent::new(script(models_script()));
    let dir = std::env::temp_dir().join(format!("chaos-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cached = h
        .core
        .probe_models(
            "fx",
            "fx 1.2.3",
            acp_adapter(&fake, None),
            std::env::temp_dir(),
            dir.clone(),
        )
        .await
        .unwrap();
    assert_eq!(cached.models.len(), 2);
    let back = chaos_core::model_cache::load(&dir, "fx").unwrap();
    assert_eq!(back, cached);
    assert_eq!(back.version, "fx 1.2.3");
    assert_eq!(
        back.freshness("fx 1.2.3", chaos_core::model_cache::now_secs()),
        chaos_core::model_cache::Freshness::Fresh
    );

    // A signed-out harness caches nothing.
    let signed_out = FakeAgent::new(script(json!({
        "fail_session_new": "Authentication required", "models": [["a", "A"]]
    })));
    let err = h
        .core
        .probe_models(
            "vibe",
            "v",
            acp_adapter(&signed_out, None),
            std::env::temp_dir(),
            dir.clone(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("Authentication required"), "{err}");
    assert!(chaos_core::model_cache::load(&dir, "vibe").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
