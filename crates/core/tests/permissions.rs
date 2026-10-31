//! Permissions by where the agent runs (STRM-401): on the host a request goes
//! to the humans without blocking the agent, and the answer comes back as a
//! wake; in the agent's own VM everything is approved.

mod common;

use std::time::Duration;

use chaos_core::{
    engine::CoreEvent,
    mcp,
    permission::{AnswerError, PermissionId, PermissionRequest, PermissionSource, PermissionState},
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

/// Reads are always allowed (ACP tool kinds `read` and `search`), on the host
/// too, and recorded nowhere; an edit or a command is declined at once with
/// the request sent to the humans. Over ACP v1 and the v2 draft.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_are_allowed_and_everything_else_is_declined_without_waiting() {
    for protocol in [1, 2] {
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
        h.say("@bot look around").await;
        let o = wait_outcomes(&h, "bot", 5).await;
        assert_eq!(
            picked(&o[0]),
            &json!("allow"),
            "read, v{protocol}: {}",
            o[0]
        );
        assert_eq!(
            picked(&o[1]),
            &json!("allow"),
            "search, v{protocol}: {}",
            o[1]
        );
        for (ix, what) in [(2, "edit"), (3, "execute"), (4, "no kind")] {
            assert_eq!(
                picked(&o[ix]),
                &json!("deny"),
                "{what}, v{protocol}: {}",
                o[ix]
            );
            let message = o[ix]["_meta"]["chaosControl"]["message"]
                .as_str()
                .unwrap_or_default();
            assert!(message.contains("read files freely"), "{message}");
        }
        // The turn carried on and ended with a reply.
        h.wait_message("the agent carried on", Some(bot.id)).await;
        // Only the three non-reads were recorded.
        let all = h.core.pending_permissions(h.main).await.unwrap();
        let titles: Vec<&str> = all.iter().map(|r| &*r.title).collect();
        assert_eq!(
            titles,
            ["Edit src/main.rs", "cargo test", "no kind given"],
            "v{protocol}"
        );
    }
}
