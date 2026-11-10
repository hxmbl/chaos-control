//! Mentions of agents that aren't in the room (STRM-561). A mention of an
//! agent that was never a member here wakes nothing and reaches nobody; it
//! stays exactly as typed. Only an agent that was a member and has been
//! removed is listed for the chat to draw as `@previous-agent`.
//! Through the real runtime with a scripted ACP agent (see `common`).

mod common;

use chaos_core::types::*;
use common::*;
use serde_json::json;

const ELSEWHERE: &str = "5f0c9d1a-7b2e-4e8f-a1c3-9d8e7f6a5b4c";

async fn triggers_for(h: &Harness, agent: MemberId) -> i64 {
    h.core
        .with_engine(move |e, _| {
            e.conn()
                .query_row(
                    "SELECT COUNT(*) FROM triggers WHERE agent_id = ?1",
                    [agent.0],
                    |r| r.get(0),
                )
                .unwrap()
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mention_of_an_agent_from_another_room_pings_nobody() {
    let mut h = Harness::new().await;
    let claude = h
        .agent(
            "claude",
            script(json!({"turns": [
                {"steps": [send_reply("here"), {"text": "Replied."}]}
            ]})),
        )
        .await;

    // codex is a member of another project's #main, with a nickname, and
    // was never in this room.
    let elsewhere = ProjectId::parse(ELSEWHERE).unwrap();
    h.core
        .ensure_project(elsewhere.clone(), "Elsewhere", None)
        .await
        .unwrap();
    let other_main = h.core.channel(&elsewhere, "main").await.unwrap().id;
    let main = h.main;
    h.main = other_main;
    let codex = h.agent("codex", script(json!({"turns": []}))).await;
    h.main = main;
    h.core.add_nickname(codex.id, "cx").await.unwrap();

    // Its handle and nickname resolve to nobody here; claude, a member,
    // is still woken by the same message.
    let ping = h.say("@codex @cx @claude can you look?").await;
    assert_eq!(ping.mentions, vec![claude.id]);
    let reply = h.wait_message("claude's reply", Some(claude.id)).await;
    assert_eq!(reply.reply_to, Some(ping.id));
    h.quiesce().await;
    assert_eq!(triggers_for(&h, codex.id).await, 0, "codex was not woken");
    assert!(h.prompts("codex").is_empty(), "codex was not prompted");

    // Nor is it listed as a former member here, so the chat leaves
    // "@codex" exactly as typed.
    assert!(
        h.core
            .former_agent_mention_tokens(h.main)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_removed_from_this_room_is_listed_and_not_woken() {
    let mut h = Harness::new().await;
    let gpt = h.agent("gpt", script(json!({"turns": []}))).await;
    h.core.add_nickname(gpt.id, "jeepy").await.unwrap();
    let claude = h.agent("claude", script(json!({"turns": []}))).await;
    assert!(
        h.core
            .former_agent_mention_tokens(h.main)
            .await
            .unwrap()
            .is_empty(),
        "nobody has left yet"
    );

    h.core.remove_member(&project_id(), gpt.id).await.unwrap();
    let former = h.core.former_agent_mention_tokens(h.main).await.unwrap();
    let former: Vec<(MemberId, String)> = former
        .into_iter()
        .map(|(m, t)| (m, t.to_string()))
        .collect();
    assert_eq!(
        former,
        vec![(gpt.id, "gpt".to_string()), (gpt.id, "jeepy".to_string())]
    );
    assert!(!former.iter().any(|(m, _)| *m == claude.id));

    // Removed, it isn't woken by its handle or nickname either.
    let ping = h.say("@gpt @jeepy still there?").await;
    assert!(ping.mentions.is_empty());
    h.quiesce().await;
    assert_eq!(triggers_for(&h, gpt.id).await, 0);
    assert!(h.prompts("gpt").is_empty());
}
