//! Nicknames (STRM-560): `@nick` wakes its agent exactly like `@handle`.
//! Through the real runtime with a scripted ACP agent (see `common`).

mod common;

use chaos_core::{mcp, types::*};
use common::*;
use serde_json::{Value, json};

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

fn names(list: Vec<std::sync::Arc<str>>) -> Vec<String> {
    list.iter().map(|n| n.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nickname_wakes_its_agent_and_stops_once_removed() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [
                {"steps": [send_reply("here"), {"text": "Replied."}]},
                {"steps": []},
                {"steps": [send_reply("again"), {"text": "Replied."}]}
            ]})),
        )
        .await;
    let added = h.core.add_nickname(bot.id, "@Botty").await.unwrap();
    assert_eq!(names(added), vec!["Botty"]);
    assert_eq!(
        names(h.core.nicknames(bot.id).await.unwrap()),
        vec!["Botty"]
    );

    // Any case of the nickname wakes it, like a handle.
    let ping = h.say("@botty are you there?").await;
    let reply = h.wait_message("bot's reply", Some(bot.id)).await;
    h.quiesce().await;
    assert_eq!(reply.reply_to, Some(ping.id));
    assert_eq!(
        h.obligation(bot.id, ping.id).await,
        Some(ObligationState::Satisfied)
    );
    let prompt = &h.wait_prompts("bot", 1).await[0];
    assert!(
        prompt.contains("You are @bot (nicknames: @Botty)"),
        "told its nickname: {prompt}"
    );
    assert!(prompt.contains("@Botty"), "{prompt}");

    // chat_context lists it for the agent and for everyone else.
    let ctx = tool(&h, bot.id, "chat_context", json!({})).await;
    assert_eq!(ctx["me"]["nicknames"], json!(["Botty"]));
    let other = h.agent("other", script(json!({"turns": []}))).await;
    let ctx = tool(&h, other.id, "chat_context", json!({})).await;
    let listed = ctx["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["handle"] == json!("bot"))
        .cloned()
        .unwrap();
    assert_eq!(listed["nicknames"], json!(["Botty"]));

    // Removed: @botty is just text again.
    let left = h.core.remove_nickname(bot.id, "BOTTY").await.unwrap();
    assert!(left.is_empty());
    let before_removed_nickname = h.count("SELECT COUNT(*) FROM triggers").await;
    let untargeted = h.say("@Botty still there?").await;
    h.quiesce().await;
    assert_eq!(
        h.count("SELECT COUNT(*) FROM triggers").await,
        before_removed_nickname + 2,
        "all agents hear the untargeted message"
    );
    assert_eq!(
        h.obligation(bot.id, untargeted.id).await,
        None,
        "the removed nickname is not a direct mention"
    );
    let prompt = &h.prompts("bot")[1];
    assert!(
        prompt.contains("posted a message"),
        "the removed nickname is ordinary message text: {prompt}"
    );
    assert_eq!(h.prompts("bot").len(), 2);
    let err = h.core.remove_nickname(bot.id, "botty").await.unwrap_err();
    assert!(err.contains("no nickname @botty"), "{err}");

    // The handle still works.
    let ping = h.say("@bot and now?").await;
    let reply = h.wait_message("bot's second reply", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(ping.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clashing_nickname_is_refused() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", script(json!({"turns": []}))).await;
    let other = h.agent("other", script(json!({"turns": []}))).await;
    h.core.add_nickname(bot.id, "Cody").await.unwrap();

    // Someone else's nickname, in any case.
    let err = h.core.add_nickname(other.id, "CODY").await.unwrap_err();
    assert!(
        err.contains("@CODY is taken") && err.contains("@bot"),
        "{err}"
    );
    // Already its own nickname.
    let err = h.core.add_nickname(bot.id, "cody").await.unwrap_err();
    assert!(err.contains("taken"), "{err}");
    // Anyone's handle, its own included, and a human's.
    let err = h.core.add_nickname(other.id, "Bot").await.unwrap_err();
    assert!(
        err.contains("@Bot is taken") && err.contains("@bot"),
        "{err}"
    );
    let err = h.core.add_nickname(bot.id, "bot").await.unwrap_err();
    assert!(err.contains("taken"), "{err}");
    let err = h.core.add_nickname(bot.id, "hx").await.unwrap_err();
    assert!(err.contains("@hx"), "{err}");
    // Not a mention token.
    for bad in ["two words", "trailing.", "-dash", "", "émile"] {
        let err = h.core.add_nickname(bot.id, bad).await.unwrap_err();
        assert!(err.starts_with("invalid nickname"), "{bad:?}: {err}");
    }
    assert!(names(h.core.nicknames(other.id).await.unwrap()).is_empty());
    assert_eq!(names(h.core.nicknames(bot.id).await.unwrap()), vec!["Cody"]);

    // @cody still means bot only.
    h.say("@cody hi").await;
    h.quiesce().await;
    assert_eq!(
        h.count("SELECT COUNT(*) FROM triggers").await,
        2,
        "two wakes now, for bot (all messages wake every agent)"
    );
    let woken: i64 = h
        .core
        .with_engine(move |e, _| {
            if let Ok(v) = e.conn().query_row("SELECT agent_id FROM triggers", [], |r| r.get::<_, i64>(0)) {
                return v;
            }
            for proj in e.store().projects().unwrap_or_default() {
                if let Ok(ag) = e.store().project_agents(&proj.id)
                    && let Ok(v) = ag.query_row("SELECT agent_id FROM triggers ORDER BY agent_id", [], |r| r.get::<_, i64>(0))
                {
                    return v;
                }
            }
            panic!("no triggers");
        })
        .await
        .unwrap();
    assert_eq!(woken, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn everyone_words_cant_be_nicknames() {
    let mut h = Harness::new().await;
    let bot = h.agent("bot", script(json!({"turns": []}))).await;
    for word in ["everyone", "@Everyone", "here", "CHANNEL"] {
        let err = h.core.add_nickname(bot.id, word).await.unwrap_err();
        assert!(err.contains("can't be nicknames"), "{word}: {err}");
    }
    assert!(h.core.nicknames(bot.id).await.unwrap().is_empty());
    let err = h
        .core
        .add_nickname(MemberId(9_999), "ghost")
        .await
        .unwrap_err();
    assert!(err.contains("no such member"), "{err}");
}
