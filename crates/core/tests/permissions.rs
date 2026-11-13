//! Permissions by where the agent runs (STRM-401): on the host a request goes
//! to the humans without blocking the agent, and the answer comes back as a
//! wake; in the agent's own VM everything is approved.

mod common;

use std::time::Duration;

use chaos_core::{
    engine::CoreEvent,
    mcp,
    permission::{
        AnswerError, LogOutcome, PermissionId, PermissionRequest, PermissionSource, PermissionState,
    },
    types::*,
};
use common::*;
use serde_json::{Value, json};

fn outcomes(h: &Harness, handle: &str) -> Vec<Value> {
    h.fake(handle)
        .observed
        .lock()
        .unwrap()
        .permission_outcomes
        .clone()
}

/// Waits (real time, bounded) until the fake agent has had `n` answers.
async fn wait_outcomes(h: &Harness, handle: &str, n: usize) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let o = outcomes(h, handle);
        if o.len() >= n {
            return o;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {n} permission answers; got {o:#?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_request(h: &mut Harness, state: PermissionState) -> PermissionRequest {
    match h
        .wait_for(
            "permission event",
            move |e| matches!(e, CoreEvent::Permission(r) if r.state == state),
        )
        .await
    {
        CoreEvent::Permission(r) => r,
        _ => unreachable!(),
    }
}

/// Waits for a system message containing `text`.
async fn wait_note(h: &mut Harness, text: &'static str) -> Message {
    match h
        .wait_for(
            text,
            move |e| matches!(e, CoreEvent::Message(m) if m.author.is_none() && m.body.contains(text)),
        )
        .await
    {
        CoreEvent::Message(m) => m,
        _ => unreachable!(),
    }
}

fn picked(outcome: &Value) -> &Value {
    &outcome["outcome"]["optionId"]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_request_goes_to_the_humans_and_an_approval_lets_the_retry_through_once() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [
                {"steps": [
                    {"permission": "rm -rf build/"},
                    {"text": "Asked to delete build/; working on the docs meanwhile."}
                ]},
                {"steps": [
                    {"permission": "rm -rf build/"},
                    {"permission": "rm -rf build/"},
                    {"text": "Deleted build/."}
                ]}
            ]})),
        )
        .await;
    let ping = h.say("@bot clean the build dir").await;

    // The request is recorded, routed to whoever woke the agent, and the
    // agent is told "not now" natively, with the request id.
    let req = wait_request(&mut h, PermissionState::Pending).await;
    assert_eq!(&*req.title, "rm -rf build/");
    assert_eq!(req.agent, bot.id);
    assert_eq!(req.asked_of, vec![h.hx.id]);
    assert_eq!(req.source, PermissionSource::Native);
    let note = wait_note(&mut h, "asked to run").await;
    assert!(
        note.body
            .contains(&format!("permission request #{}, sent to @hx", req.id)),
        "{}",
        note.body
    );
    let first = wait_outcomes(&h, "bot", 1).await;
    assert_eq!(picked(&first[0]), &json!("deny"), "{}", first[0]);
    assert_eq!(
        first[0]["_meta"]["chaosControl"]["requestId"],
        json!(req.id.0)
    );

    // Non-blocking: the turn carried on and finished.
    let reply = h.wait_message("the agent carried on", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(ping.id));
    let status = h.core.status(bot.id, h.main).await.unwrap();
    assert_ne!(
        status.glow,
        Glow::Unresponsive,
        "a pending request is not a failure"
    );
    let presence = status.presence.unwrap_or_default();
    assert!(presence.contains("request pending"), "{presence}");
    assert!(!presence.contains("blocked"), "{presence}");

    // A human approves; the answer is posted and wakes the agent.
    let answered = h
        .core
        .answer_permission(req.id, h.hx.id, true, Some("go ahead"))
        .await
        .unwrap();
    assert_eq!(answered.state, PermissionState::Approved);
    assert_eq!(answered.answered_by, Some(h.hx.id));
    let note = wait_note(&mut h, "approved").await;
    assert!(note.body.contains("\"go ahead\""), "{}", note.body);
    let prompts = h.wait_prompts("bot", 2).await;
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[1].contains("answered your permission request"),
        "{}",
        prompts[1]
    );
    assert!(prompts[1].contains("approved"), "{}", prompts[1]);

    // The retry goes through once; a second identical call is a new request.
    let all = wait_outcomes(&h, "bot", 3).await;
    assert_eq!(picked(&all[1]), &json!("allow"), "{}", all[1]);
    assert_eq!(picked(&all[2]), &json!("deny"), "{}", all[2]);
    assert_ne!(
        all[2]["_meta"]["chaosControl"]["requestId"],
        json!(req.id.0)
    );
    h.until("grant used", move |c| {
        c.query_row(
            "SELECT used_at IS NOT NULL FROM permission_requests WHERE id = ?1",
            [req.id.0],
            |r| r.get::<_, bool>(0),
        )
        .unwrap()
    })
    .await;

    // Answering twice is refused.
    let again = h
        .core
        .answer_permission(req.id, h.hx.id, false, None)
        .await
        .unwrap_err();
    assert_eq!(
        again,
        AnswerError::AlreadyAnswered(PermissionState::Approved)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denial_is_an_answer_not_a_grant_or_a_failure() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [
                {"steps": [{"permission": "git push --force"}, {"text": "Asked."}]},
                {"steps": [{"permission": "git push --force"}, {"text": "Understood."}]}
            ]})),
        )
        .await;
    h.say("@bot push it").await;
    let req = wait_request(&mut h, PermissionState::Pending).await;
    h.wait_message("first turn", Some(bot.id)).await;
    let denied = h
        .core
        .answer_permission(req.id, h.hx.id, false, None)
        .await
        .unwrap();
    assert_eq!(denied.state, PermissionState::Denied);
    let note = wait_note(&mut h, "denied").await;
    assert!(note.body.contains("not a failure"), "{}", note.body);
    let all = wait_outcomes(&h, "bot", 2).await;
    assert_eq!(picked(&all[1]), &json!("deny"), "a denial grants nothing");
    let status = h.core.status(bot.id, h.main).await.unwrap();
    assert_ne!(status.glow, Glow::Unresponsive);
}

/// In `yolo` mode, mediated host requests are allowed by policy (recorded as
/// auto-approved-by-mode), preserving existing yolo semantics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_mode_allows_mediated_requests_without_human_approval() {
    for protocol in [1, 2] {
        let mut h = Harness::new().await;
        let bot = h
            .agent(
                "bot",
                script(json!({"protocol": protocol, "turns": [{"steps": [
                    {"permission_kind": {"title": "Read src/main.rs", "kind": "read"}},
                    {"permission_kind": {"title": "Edit src/main.rs", "kind": "edit"}},
                    {"permission": "run something"},
                    {"text": "Done."}
                ]}]})),
            )
            .await;
        h.core
            .set_approval_mode(bot.id, ApprovalMode::Yolo)
            .await
            .unwrap();
        h.say("@bot go").await;
        let all = wait_outcomes(&h, "bot", 3).await;
        for (ix, what) in [(0, "read"), (1, "edit"), (2, "other")] {
            assert_eq!(
                picked(&all[ix]),
                &json!("allow"),
                "{what}, v{protocol}: {}",
                all[ix]
            );
        }
        h.wait_message("reply", Some(bot.id)).await;
        let pending = h.core.pending_permissions(h.main).await.unwrap();
        assert!(pending.is_empty(), "v{protocol}: {pending:#?}");
        let log = h.core.permission_log(h.main, 50).await.unwrap();
        assert_eq!(log.len(), 3, "v{protocol}: {log:#?}");
        assert!(log.iter().all(|r| {
            r.state == PermissionState::AutoApproved
                && r.decided_by == Some(DecidedBy::AutoMode)
                && r.outcome() == LogOutcome::AutoAllowed
        }));
    }
}

/// A VM agent's permission requests are approved without asking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_in_its_own_vm_is_approved_without_asking() {
    let mut h = Harness::new().await;
    let bot = h
        .agent_in_vm(
            "bot",
            script(json!({"turns": [{"steps": [
                {"permission": "rm -rf /tmp/scratch"},
                {"text": "Done."}
            ]}]})),
        )
        .await;
    h.say("@bot clean scratch").await;
    let req = wait_request(&mut h, PermissionState::AutoApproved).await;
    assert!(req.asked_of.is_empty());
    let reply = h.wait_message("reply", Some(bot.id)).await;
    assert_eq!(&*reply.body, "Done.");
    let all = outcomes(&h, "bot");
    assert_eq!(picked(&all[0]), &json!("allow"), "{}", all[0]);
    assert_eq!(
        h.count("SELECT COUNT(*) FROM messages WHERE origin = 'system'")
            .await,
        0,
        "nobody is asked, so nothing is posted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_tools_ask_ahead_without_blocking_and_poll_the_answer() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", script(json!({"turns": []}))).await;
    let scope = mcp::Scope {
        agent: bot.id,
        channel: h.main,
        epoch: 0,
    };
    let ask = json!({"action": "deploy to prod", "reason": "release 1.2"});
    let sent = h
        .core
        .tool(scope, "chat_request_permission", ask.clone())
        .await
        .unwrap();
    assert_eq!(sent["status"], json!("request_sent"), "{sent}");
    assert_eq!(sent["asked"], json!(["@hx"]));
    let id = sent["request_id"].as_i64().unwrap();
    // Asking again doesn't send the humans a second request.
    let again = h
        .core
        .tool(scope, "chat_request_permission", ask.clone())
        .await
        .unwrap();
    assert_eq!(again["request_id"], json!(id));
    let status = h
        .core
        .tool(scope, "chat_permission_status", json!({}))
        .await
        .unwrap();
    assert_eq!(status["requests"].as_array().unwrap().len(), 1);
    assert_eq!(status["requests"][0]["state"], json!("pending"));
    let note = wait_note(&mut h, "asked to run").await;
    assert!(note.body.contains("(release 1.2)"), "{}", note.body);

    // Only a human in the channel can answer.
    let by_agent = h
        .core
        .answer_permission(PermissionId(id), bot.id, true, None)
        .await
        .unwrap_err();
    assert_eq!(by_agent, AnswerError::NotAllowed);
    assert_eq!(
        h.core
            .answer_permission(PermissionId(9_999), h.hx.id, true, None)
            .await
            .unwrap_err(),
        AnswerError::UnknownRequest
    );

    h.core
        .answer_permission(PermissionId(id), h.hx.id, true, None)
        .await
        .unwrap();
    let one = h
        .core
        .tool(scope, "chat_permission_status", json!({"request_id": id}))
        .await
        .unwrap();
    assert_eq!(one["state"], json!("approved"), "{one}");
    assert_eq!(one["answered_by"], json!("@hx"));
    assert_eq!(one["used"], json!(false));
    // Asking ahead for something already approved says so.
    let ok = h
        .core
        .tool(scope, "chat_request_permission", ask)
        .await
        .unwrap();
    assert_eq!(ok["status"], json!("approved"), "{ok}");
    assert_eq!(ok["request_id"], json!(id));
    assert!(h.core.pending_permissions(h.main).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vm_agent_asking_ahead_is_told_to_go_ahead() {
    let mut h = Harness::new().await;
    let bot = h.agent_in_vm("bot", script(json!({"turns": []}))).await;
    let scope = mcp::Scope {
        agent: bot.id,
        channel: h.main,
        epoch: 0,
    };
    let r = h
        .core
        .tool(
            scope,
            "chat_request_permission",
            json!({"action": "install deps"}),
        )
        .await
        .unwrap();
    assert_eq!(r["status"], json!("approved"), "{r}");
    assert_eq!(r["state"], json!("auto_approved"));
}

/// Reads and local search (ACP kinds `read` and `search`) require human
/// approval on the mediated host path in both `ask-all` and `accept-edits`
/// (STRM-596/606). The adapter answers "not now" at once with a request sent
/// to humans; the agent can continue, and a human decision arrives later.
/// Over ACP v1 and the v2 draft.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_and_local_search_require_human_approval_without_blocking() {
    for protocol in [1, 2] {
        for mode in [ApprovalMode::AskAll, ApprovalMode::AcceptEdits] {
            let mut h = Harness::new().await;
            let bot = h
                .agent(
                    "bot",
                    script(json!({"protocol": protocol, "turns": [{"steps": [
                        {"permission_kind": {"title": "Read src/main.rs", "kind": "read"}},
                        {"permission_kind": {"title": "Search for TODO", "kind": "search"}},
                        {"permission_kind": {"title": "Edit src/main.rs", "kind": "edit"}},
                        {"permission_kind": {"title": "cargo test", "kind": "execute"}},
                        {"permission": "no kind given"},
                        {"text": "Read what I could."}
                    ]}]})),
                )
                .await;
            h.core.set_approval_mode(bot.id, mode).await.unwrap();
            h.say("@bot look around").await;
            let o = wait_outcomes(&h, "bot", 5).await;
            for (ix, what) in [
                (0, "read"),
                (1, "search"),
                (2, "edit"),
                (3, "execute"),
                (4, "no kind"),
            ] {
                assert_eq!(
                    picked(&o[ix]),
                    &json!("deny"),
                    "{what}, v{protocol}, {mode:?}: {}",
                    o[ix]
                );
                assert_eq!(
                    o[ix]["_meta"]["chaosControl"]["status"],
                    json!("request_sent"),
                    "{what}, v{protocol}, {mode:?}: {}",
                    o[ix]
                );
                assert!(
                    o[ix]["_meta"]["chaosControl"]["requestId"]
                        .as_i64()
                        .is_some(),
                    "{what}, v{protocol}, {mode:?}: {}",
                    o[ix]
                );
            }
            // The turn carried on and ended with a reply.
            h.wait_message("the agent carried on", Some(bot.id)).await;
            // Mediated requests in this slice went to the humans. pending_permissions()
            // returns records whose human-answer state is still pending; that is
            // intentionally separate from execution/run lifecycle surfaced via run_state
            // and outcome() in the log/queue views.
            let all = h.core.pending_permissions(h.main).await.unwrap();
            let titles: Vec<&str> = all.iter().map(|r| &*r.title).collect();
            assert_eq!(
                titles,
                [
                    "Read src/main.rs",
                    "Search for TODO",
                    "Edit src/main.rs",
                    "cargo test",
                    "no kind given",
                ],
                "v{protocol}, {mode:?}"
            );
            assert!(all.iter().all(|r| r.state == PermissionState::Pending));
            // The permission log records the same mediated requests (not auto-allowed).
            // In accept-edits, execute remains Queue-preserved: without a safely
            // capturable command/cwd it is stored pending for human decision but
            // marked run_state=unrunnable, so outcome() reports Failed while DB state
            // remains Pending. Read/search and generic edit are AskHuman (fail closed).
            let log = h.core.permission_log(h.main, 50).await.unwrap();
            let rows: Vec<(&str, LogOutcome, bool, PermissionState, Option<&str>)> = log
                .iter()
                .map(|r| {
                    (
                        &*r.title,
                        r.outcome(),
                        r.read_only,
                        r.state,
                        r.run_state.as_deref(),
                    )
                })
                .collect();
            let expected = match mode {
                ApprovalMode::AskAll => vec![
                    (
                        "no kind given",
                        LogOutcome::Pending,
                        false,
                        PermissionState::Pending,
                        None,
                    ),
                    (
                        "cargo test",
                        LogOutcome::Pending,
                        false,
                        PermissionState::Pending,
                        None,
                    ),
                    (
                        "Edit src/main.rs",
                        LogOutcome::Pending,
                        false,
                        PermissionState::Pending,
                        None,
                    ),
                    (
                        "Search for TODO",
                        LogOutcome::Pending,
                        false,
                        PermissionState::Pending,
                        None,
                    ),
                    (
                        "Read src/main.rs",
                        LogOutcome::Pending,
                        false,
                        PermissionState::Pending,
                        None,
                    ),
                ],
                ApprovalMode::AcceptEdits => vec![
                    (
                        "no kind given",
                        LogOutcome::Pending,
                        false,
                        PermissionState::Pending,
                        None,
                    ),
                    (
                        "cargo test",
                        LogOutcome::Failed,
                        false,
                        PermissionState::Pending,
                        Some("unrunnable"),
                    ),
                    (
                        "Edit src/main.rs",
                        LogOutcome::Pending,
                        false,
                        PermissionState::Pending,
                        None,
                    ),
                    (
                        "Search for TODO",
                        LogOutcome::Pending,
                        false,
                        PermissionState::Pending,
                        None,
                    ),
                    (
                        "Read src/main.rs",
                        LogOutcome::Pending,
                        false,
                        PermissionState::Pending,
                        None,
                    ),
                ],
                ApprovalMode::Yolo => unreachable!("yolo not exercised here"),
            };
            assert_eq!(rows, expected, "v{protocol}, {mode:?}");
            assert!(log.iter().all(|r| r.agent == bot.id));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_chat_tools_are_auto_allowed_and_logged_by_name() {
    for protocol in [1, 2] {
        let mut h = Harness::new().await;
        let bot = h
            .agent(
                "bot",
                script(json!({"protocol": protocol, "turns": [{"steps": [
                    {"permission_kind": {"title": "a tool", "name": "chat_send", "kind": "execute"}},
                    {"permission_kind": {"title": "a tool", "name": "external_tool", "kind": "execute"}},
                    {"text": "The chat tool ran; the other tool needs approval."}
                ]}]})),
            )
            .await;
        h.say("@bot use the chat tool").await;

        let all = wait_outcomes(&h, "bot", 2).await;
        assert_eq!(
            picked(&all[0]),
            &json!("allow"),
            "chat tool, v{protocol}: {}",
            all[0]
        );
        assert_eq!(
            picked(&all[1]),
            &json!("deny"),
            "other tool, v{protocol}: {}",
            all[1]
        );
        h.wait_message("the agent carried on", Some(bot.id)).await;

        let pending = h.core.pending_permissions(h.main).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(&*pending[0].title, "external_tool");
        let log = h.core.permission_log(h.main, 50).await.unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(&*log[0].title, "external_tool");
        assert_eq!(log[0].state, PermissionState::Pending);
        assert_eq!(&*log[1].title, "chat_send");
        assert_eq!(log[1].state, PermissionState::AutoApproved);
        assert!(!log[1].read_only);
    }
}

/// Fetch/URL-open-like requests are not granted a "verified web-search
/// (search-only)" exception without verifiable identity/scope/search-only proof
/// on the production path (STRM-596/606). Generic kind/name/title metadata is
/// insufficient, so they remain approval-gated in both `ask-all` and
/// `accept-edits`. Over ACP v1/v2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unverified_fetch_search_like_requests_require_human_approval() {
    for protocol in [1, 2] {
        for mode in [ApprovalMode::AskAll, ApprovalMode::AcceptEdits] {
            let mut h = Harness::new().await;
            let req = json!({"permission_kind": {
                "title": "Search the web for docs",
                "name": "web_search",
                "kind": "fetch",
                "raw_input": {"url": "https://example.com/search?q=docs"}
            }});
            let bot = h
                .agent(
                    "bot",
                    script(json!({"protocol": protocol, "turns": [{
                        "steps": [req.clone(), {"text": "Asked."}]
                    }]})),
                )
                .await;
            h.core.set_approval_mode(bot.id, mode).await.unwrap();

            h.say("@bot search docs").await;
            let pending = wait_request(&mut h, PermissionState::Pending).await;
            assert_eq!(
                &*pending.title, "Search the web for docs",
                "v{protocol}, {mode:?}"
            );
            assert!(
                pending.run_state.is_none() && pending.command.is_none(),
                "fetch must not be queued as command execution, v{protocol}, {mode:?}: {pending:#?}"
            );

            let outcomes = wait_outcomes(&h, "bot", 1).await;
            assert_eq!(
                picked(&outcomes[0]),
                &json!("deny"),
                "v{protocol}, {mode:?}: {}",
                outcomes[0]
            );
            assert_eq!(
                outcomes[0]["_meta"]["chaosControl"]["status"],
                json!("request_sent"),
                "v{protocol}, {mode:?}: {}",
                outcomes[0]
            );

            h.wait_message("turn completed", Some(bot.id)).await;
            let all = h.core.pending_permissions(h.main).await.unwrap();
            assert_eq!(all.len(), 1, "v{protocol}, {mode:?}: {all:#?}");
            assert_eq!(all[0].state, PermissionState::Pending);
        }
    }
}

/// Shell/execute requests (including read-only-looking commands) require human
/// approval in both `ask-all` and `accept-edits` (STRM-596). In `ask-all`
/// approval yields a one-shot retry grant; in `accept-edits` execute is queued
/// for approval-gated command execution (not auto-allowed). Over ACP v1/v2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_execute_requires_human_approval_in_ask_all_and_accept_edits() {
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    for protocol in [1, 2] {
        for mode in [ApprovalMode::AskAll, ApprovalMode::AcceptEdits] {
            let mut h = Harness::new().await;
            let cmd = json!({"permission_kind": {
                "title": "ls -la",
                "kind": "execute",
                "raw_input": {"command": "ls -la", "cwd": cwd}
            }});
            let bot = h
                .agent(
                    "bot",
                    script(json!({"protocol": protocol, "turns": [
                        {"steps": [cmd.clone(), {"text": "Asked."}]},
                        {"steps": [cmd.clone(), {"text": "Done."}]}
                    ]})),
                )
                .await;
            h.core.set_approval_mode(bot.id, mode).await.unwrap();

            h.say("@bot list files").await;
            let req = wait_request(&mut h, PermissionState::Pending).await;
            assert_eq!(&*req.title, "ls -la", "v{protocol}, {mode:?}");
            let first = wait_outcomes(&h, "bot", 1).await;
            assert_eq!(
                picked(&first[0]),
                &json!("deny"),
                "v{protocol}, {mode:?}: {}",
                first[0]
            );
            assert_eq!(
                first[0]["_meta"]["chaosControl"]["status"],
                json!("request_sent"),
                "v{protocol}, {mode:?}: {}",
                first[0]
            );
            h.wait_message("first turn", Some(bot.id)).await;

            let stored = h
                .core
                .with_engine(move |e, _| chaos_core::permission::get(e.conn(), req.id).unwrap())
                .await
                .unwrap()
                .unwrap();
            match mode {
                ApprovalMode::AskAll => {
                    assert_eq!(stored.run_state, None, "v{protocol}");
                }
                ApprovalMode::AcceptEdits => {
                    // Queued for approval-gated execution, not auto-allowed.
                    assert_eq!(
                        stored.run_state.as_deref(),
                        Some("queued"),
                        "v{protocol}: {stored:#?}"
                    );
                }
                ApprovalMode::Yolo => unreachable!(),
            }

            // Deny: no grant/execution; retry is a new pending request.
            h.core
                .answer_permission(req.id, h.hx.id, false, None)
                .await
                .unwrap();
            let all = wait_outcomes(&h, "bot", 2).await;
            assert_eq!(
                picked(&all[1]),
                &json!("deny"),
                "v{protocol}, {mode:?}: {}",
                all[1]
            );
            assert_ne!(
                all[1]["_meta"]["chaosControl"]["requestId"],
                json!(req.id.0),
                "v{protocol}, {mode:?}"
            );
            let pending = h.core.pending_permissions(h.main).await.unwrap();
            assert!(!pending.is_empty(), "v{protocol}, {mode:?}");
        }
    }
}

/// In `accept-edits`, mediated `fetch` and unknown/`other` requests (ACP v1/v2)
/// must reach the human approval flow: pending requests with request_sent
/// responses, not auto-approved, not silently allowed, and not queued for
/// command execution. Denial grants nothing; approval is one-shot for the exact
/// request key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept_edits_fetch_and_other_require_human_approval_with_one_shot_lifecycle() {
    for protocol in [1, 2] {
        let mut h = Harness::new().await;
        let fetch = json!({"permission_kind": {"title": "Fetch https://example.com/data", "kind": "fetch"}});
        let other = json!({"permission_kind": {"title": "Unknown tool action", "kind": "other"}});
        let bot = h
            .agent(
                "bot",
                script(json!({"protocol": protocol, "turns": [
                    {"steps": [fetch.clone(), other.clone(), {"text": "Asked."}]},
                    {"steps": [fetch.clone(), other.clone(), other.clone(), {"text": "Done."}]}
                ]})),
            )
            .await;
        h.core
            .set_approval_mode(bot.id, ApprovalMode::AcceptEdits)
            .await
            .unwrap();

        h.say("@bot run fetch and other").await;
        let req_fetch = wait_request(&mut h, PermissionState::Pending).await;
        let req_other = wait_request(&mut h, PermissionState::Pending).await;
        assert_eq!(
            &*req_fetch.title, "Fetch https://example.com/data",
            "v{protocol}"
        );
        assert_eq!(&*req_other.title, "Unknown tool action", "v{protocol}");
        for req in [&req_fetch, &req_other] {
            assert_eq!(req.state, PermissionState::Pending, "v{protocol}: {req:#?}");
            assert!(
                req.run_state.is_none(),
                "not queued/command execution, v{protocol}: {req:#?}"
            );
            assert!(req.command.is_none(), "v{protocol}: {req:#?}");
            assert!(req.cwd.is_none(), "v{protocol}: {req:#?}");
        }

        let first = wait_outcomes(&h, "bot", 2).await;
        for (ix, what) in [(0, "fetch"), (1, "other")] {
            assert_eq!(
                picked(&first[ix]),
                &json!("deny"),
                "{what}, v{protocol}: {}",
                first[ix]
            );
            assert_eq!(
                first[ix]["_meta"]["chaosControl"]["status"],
                json!("request_sent"),
                "{what}, v{protocol}: {}",
                first[ix]
            );
            assert!(
                first[ix]["_meta"]["chaosControl"]["requestId"]
                    .as_i64()
                    .is_some(),
                "{what}, v{protocol}: {}",
                first[ix]
            );
        }

        h.wait_message("first turn", Some(bot.id)).await;

        h.core
            .answer_permission(req_fetch.id, h.hx.id, false, None)
            .await
            .unwrap();
        h.core
            .answer_permission(req_other.id, h.hx.id, true, None)
            .await
            .unwrap();

        let all = wait_outcomes(&h, "bot", 5).await;
        let id = |o: &serde_json::Value| o["_meta"]["chaosControl"]["requestId"].clone();
        // Denied fetch: retry grants nothing and creates a new pending request.
        assert_eq!(
            picked(&all[2]),
            &json!("deny"),
            "fetch retry, v{protocol}: {}",
            all[2]
        );
        assert_ne!(id(&all[2]), json!(req_fetch.id.0), "v{protocol}");
        // Approved other: exact retry allowed once.
        assert_eq!(
            picked(&all[3]),
            &json!("allow"),
            "other retry, v{protocol}: {}",
            all[3]
        );
        // Second identical other retry after grant consumed: new pending request.
        assert_eq!(
            picked(&all[4]),
            &json!("deny"),
            "other again, v{protocol}: {}",
            all[4]
        );
        assert_ne!(id(&all[4]), json!(req_other.id.0), "v{protocol}");
        assert_ne!(id(&all[4]), id(&all[2]), "v{protocol}");

        h.until("approved other grant used", move |c| {
            c.query_row(
                "SELECT used_at IS NOT NULL FROM permission_requests WHERE id = ?1",
                [req_other.id.0],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
        })
        .await;

        let pending = h.core.pending_permissions(h.main).await.unwrap();
        assert_eq!(pending.len(), 2, "v{protocol}: {pending:#?}");
        let titles: Vec<&str> = pending.iter().map(|r| &*r.title).collect();
        assert_eq!(
            titles,
            ["Fetch https://example.com/data", "Unknown tool action"],
            "v{protocol}"
        );
        assert!(
            pending
                .iter()
                .all(|r| r.run_state.is_none() && r.command.is_none())
        );
    }
}

/// A grant is for the exact request, not its title (STRM-572): after a human
/// approves "Edit files" on a.rs, "Edit files" on b.rs is still declined,
/// the identical a.rs call goes through once (with a new tool call id), and
/// a second identical call is a new request. Over ACP v1 and the v2 draft.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_is_for_the_exact_request_not_its_title() {
    for protocol in [1, 2] {
        let mut h = Harness::new().await;
        let edit = |path: &str| {
            json!({"permission_kind": {
                "title": "Edit files", "kind": "edit", "raw_input": {"path": path}
            }})
        };
        let bot = h
            .agent(
                "bot",
                script(json!({"protocol": protocol, "turns": [
                    {"steps": [edit("a.rs"), {"text": "Asked to edit a.rs."}]},
                    {"steps": [edit("b.rs"), edit("a.rs"), edit("a.rs"), {"text": "Edited a.rs."}]}
                ]})),
            )
            .await;
        h.say("@bot fix a.rs").await;
        let req = wait_request(&mut h, PermissionState::Pending).await;
        assert_eq!(&*req.title, "Edit files");
        let key = req.request_key.clone().expect("a native request has a key");
        assert!(key.starts_with("edit:"), "v{protocol}: {key}");
        h.wait_message("first turn", Some(bot.id)).await;

        h.core
            .answer_permission(req.id, h.hx.id, true, None)
            .await
            .unwrap();
        let all = wait_outcomes(&h, "bot", 4).await;
        let id = |o: &Value| o["_meta"]["chaosControl"]["requestId"].clone();
        assert_eq!(picked(&all[0]), &json!("deny"), "v{protocol}: {}", all[0]);
        // Same title, another file: declined, and sent as its own request.
        assert_eq!(
            picked(&all[1]),
            &json!("deny"),
            "b.rs, v{protocol}: {}",
            all[1]
        );
        assert_ne!(id(&all[1]), json!(req.id.0), "v{protocol}");
        // The identical call (a new tool call id) uses the grant, once.
        assert_eq!(
            picked(&all[2]),
            &json!("allow"),
            "a.rs, v{protocol}: {}",
            all[2]
        );
        assert_eq!(
            picked(&all[3]),
            &json!("deny"),
            "again, v{protocol}: {}",
            all[3]
        );
        assert_ne!(id(&all[3]), json!(req.id.0), "v{protocol}");
        assert_ne!(id(&all[3]), id(&all[1]), "v{protocol}");
        h.until("grant used", move |c| {
            c.query_row(
                "SELECT used_at IS NOT NULL FROM permission_requests WHERE id = ?1",
                [req.id.0],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
        })
        .await;
        // b.rs and the second a.rs wait for the humans, with their own keys.
        let pending = h.core.pending_permissions(h.main).await.unwrap();
        assert_eq!(pending.len(), 2, "v{protocol}: {pending:#?}");
        assert!(pending.iter().all(|r| &*r.title == "Edit files"));
        assert_ne!(
            pending[0].request_key,
            Some(key.clone()),
            "b.rs, v{protocol}"
        );
        assert_eq!(pending[1].request_key, Some(key), "a.rs again, v{protocol}");
    }
}
