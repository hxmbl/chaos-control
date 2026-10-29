//! Emoji reactions (STRM-467): add and remove are idempotent, a reaction
//! never wakes anyone (not even the message's author), a pinged agent's
//! reaction closes its obligation as handled rather than ignored, and agents
//! see reactions in `chat_read`, `chat_context` and their next prompt.

mod common;

use chaos_core::{engine::CoreEvent, engine::PostError, mcp, types::*};
use common::*;
use serde_json::{Value, json};

fn react(emoji: &str) -> Value {
    json!({"mcp": {"tool": "chat_react", "args": {"message_id": "$TRIGGER", "emoji": emoji}}})
}

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

async fn settled(h: &mut Harness, n: i64) {
    h.until("batches settled", move |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'settled'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == n
    })
    .await;
}

async fn resolved_kind(h: &Harness, message: MessageId) -> Option<String> {
    h.core
        .with_engine(move |e, _| {
            e.conn()
                .query_row(
                    "SELECT resolved_kind FROM obligations WHERE message_id = ?1",
                    [message.0],
                    |r| r.get(0),
                )
                .unwrap()
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn any_reaction_from_a_pinged_agent_closes_the_obligation_as_handled() {
    let mut h = Harness::new().await;
    // 💀 and nothing else: handled, not ignored, and nothing posted.
    let skull = h
        .agent(
            "skull",
            script(json!({"turns": [{"steps": [react("💀")]}]})),
        )
        .await;
    let ping = h.say("@skull I broke CI again").await;
    h.wait_for(
        "skull reaction",
        |e| matches!(e, CoreEvent::Reaction { member, added: true, .. } if *member == skull.id),
    )
    .await;
    settled(&mut h, 1).await;
    assert_eq!(
        h.obligation(skull.id, ping.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(resolved_kind(&h, ping.id).await.as_deref(), Some("react"));
    assert!(h.by(skull.id).await.is_empty(), "a reaction posts nothing");
    let st = h.core.status(skull.id, h.main).await.unwrap();
    assert_ne!(
        st.presence.as_deref(),
        Some("read it and let it go"),
        "reacted is not ignored"
    );

    // 👀 ("on it") holds the obligation during the turn; with no final text
    // the turn still counts as handled, not ignored.
    let eyes = h
        .agent(
            "eyes",
            script(json!({"turns": [{"steps": [react(":eyes:")]}]})),
        )
        .await;
    let ping2 = h.say("@eyes can you look").await;
    settled(&mut h, 2).await;
    assert_eq!(
        h.obligation(eyes.id, ping2.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(resolved_kind(&h, ping2.id).await.as_deref(), Some("react"));

    // No reaction and no text is still `ignored`.
    let mute = h
        .agent("mute", script(json!({"turns": [{"steps": []}]})))
        .await;
    let ping3 = h.say("@mute hello?").await;
    settled(&mut h, 3).await;
    assert_eq!(
        h.obligation(mute.id, ping3.id).await,
        Some(ObligationState::Ignored)
    );

    // Taking the reaction back later doesn't reopen anything.
    h.core.react(skull.id, ping.id, "💀", false).await.unwrap();
    assert_eq!(
        h.obligation(skull.id, ping.id).await,
        Some(ObligationState::Satisfied)
    );
    h.advance(10 * 60 * 1000).await;
    assert_eq!(h.prompts("skull").len(), 1, "never retried");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reaction_wakes_nobody_not_even_the_author() {
    let mut h = Harness::new().await;
    let poster = h
        .agent(
            "poster",
            script(json!({"turns": [{"steps": [send_reply("Shipped.")]}, {"steps": []}]})),
        )
        .await;
    let other = h
        .agent("other", script(json!({"turns": [{"steps": []}]})))
        .await;
    h.say("@poster ship it").await;
    let shipped = h.wait_message("poster's reply", Some(poster.id)).await;
    settled(&mut h, 1).await;
    h.quiesce().await;
    let batches = h.count("SELECT COUNT(*) FROM batches").await;
    let triggers = h.count("SELECT COUNT(*) FROM triggers").await;

    // A human and another agent react to the poster's message; the poster
    // is mentioned nowhere and replied to by no one.
    h.core.react(h.hx.id, shipped.id, "🎉", true).await.unwrap();
    h.core
        .react(other.id, shipped.id, "💀", true)
        .await
        .unwrap();
    // A reaction on a message that mentions an agent wakes that agent no
    // more than any other.
    let mention = h
        .core
        .post(poster.id, h.main, "thanks @other", None)
        .await
        .unwrap();
    settled(&mut h, 2).await; // @other's own wake from the mention
    let after_mention_batches = h.count("SELECT COUNT(*) FROM batches").await;
    let after_mention_triggers = h.count("SELECT COUNT(*) FROM triggers").await;
    h.core.react(h.hx.id, mention.id, "👍", true).await.unwrap();
    h.advance(60_000).await;

    assert_eq!(after_mention_batches, batches + 1);
    assert_eq!(after_mention_triggers, triggers + 1);
    assert_eq!(
        h.count("SELECT COUNT(*) FROM batches").await,
        after_mention_batches,
        "no batch for any reaction"
    );
    assert_eq!(
        h.count("SELECT COUNT(*) FROM triggers").await,
        after_mention_triggers,
        "no trigger for any reaction"
    );
    assert_eq!(h.prompts("poster").len(), 1, "the author is not woken");
    assert_eq!(h.prompts("other").len(), 1, "only the mention woke @other");
    let st = h.core.status(poster.id, h.main).await.unwrap();
    assert_ne!(st.glow, Glow::Unresponsive);
    // The UI still hears about each one.
    for member in [h.hx.id, other.id] {
        h.wait_for(
            "reaction event",
            |e| matches!(e, CoreEvent::Reaction { member: m, message, added: true, .. } if *m == member && *message == shipped.id),
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_and_remove_are_idempotent() {
    let mut h = Harness::new().await;
    let m = h.say("ship it").await;
    let first = h.core.react(h.hx.id, m.id, "👍", true).await.unwrap();
    assert!(first.changed);
    // The same emoji again, and its shortcode: no change, no event.
    let again = h.core.react(h.hx.id, m.id, ":+1:", true).await.unwrap();
    assert!(!again.changed);
    assert_eq!(&*again.emoji, "👍");
    assert_eq!(h.count("SELECT COUNT(*) FROM reactions").await, 1);
    // A second, different emoji from the same member is its own reaction.
    assert!(
        h.core
            .react(h.hx.id, m.id, "💀", true)
            .await
            .unwrap()
            .changed
    );
    assert_eq!(h.count("SELECT COUNT(*) FROM reactions").await, 2);

    assert!(
        h.core
            .react(h.hx.id, m.id, "👍", false)
            .await
            .unwrap()
            .changed
    );
    assert!(
        !h.core
            .react(h.hx.id, m.id, "👍", false)
            .await
            .unwrap()
            .changed
    );
    assert_eq!(h.count("SELECT COUNT(*) FROM reactions").await, 1);

    h.quiesce().await;
    h.drain();
    let events: Vec<(bool, String)> = h
        .seen
        .iter()
        .filter_map(|e| match e {
            CoreEvent::Reaction { emoji, added, .. } => Some((*added, emoji.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(
        events,
        vec![
            (true, "👍".to_string()),
            (true, "💀".to_string()),
            (false, "👍".to_string())
        ],
        "one event per real change"
    );

    // Not an emoji, or not something this member can see.
    assert_eq!(
        h.core.react(h.hx.id, m.id, "nice", true).await.unwrap_err(),
        PostError::InvalidEmoji
    );
    assert_eq!(
        h.core
            .react(h.hx.id, MessageId(9_999), "👍", true)
            .await
            .unwrap_err(),
        PostError::UnknownMessage
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_see_reactions_in_read_context_and_tools() {
    let mut h = Harness::new().await;
    let bot = h
        .agent("bot", script(json!({"turns": [{"steps": []}]})))
        .await;
    let m = h.say("deploy is green").await;
    h.core.react(h.hx.id, m.id, "✅", true).await.unwrap();

    // chat_react adds and removes, and reports the message's reactions.
    let added = tool(
        &h,
        bot.id,
        "chat_react",
        json!({"message_id": m.id.0, "emoji": "✅"}),
    )
    .await;
    assert_eq!(added["changed"], json!(true));
    assert_eq!(
        added["reactions"],
        json!([{"emoji": "✅", "count": 2, "by": ["hx", "bot"]}])
    );
    let removed = tool(
        &h,
        bot.id,
        "chat_react",
        json!({"message_id": m.id.0, "emoji": "✅", "remove": true}),
    )
    .await;
    assert_eq!(removed["changed"], json!(true));
    assert_eq!(removed["removed"], json!(true));
    let removed_again = tool(
        &h,
        bot.id,
        "chat_react",
        json!({"message_id": m.id.0, "emoji": "✅", "remove": true}),
    )
    .await;
    assert_eq!(removed_again["changed"], json!(false));
    tool(
        &h,
        bot.id,
        "chat_react",
        json!({"message_id": m.id.0, "emoji": "skull"}),
    )
    .await;

    let read = tool(&h, bot.id, "chat_read", json!({"after_seq": 0})).await;
    let view = read["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == json!(m.id.0))
        .unwrap()
        .clone();
    assert_eq!(
        view["reactions"],
        json!([
            {"emoji": "✅", "count": 1, "by": ["hx"]},
            {"emoji": "💀", "count": 1, "by": ["bot"]}
        ])
    );

    let ctx = tool(&h, bot.id, "chat_context", json!({})).await;
    let recent = ctx["recent_reactions"].as_array().unwrap();
    let summaries: Vec<&str> = recent
        .iter()
        .map(|r| r["summary"].as_str().unwrap())
        .collect();
    assert_eq!(
        summaries,
        vec![
            format!("Hx reacted ✅ to #{}", m.seq),
            format!("bot reacted 💀 to #{}", m.seq)
        ]
    );

    // A word is refused with a readable error.
    let scope = mcp::Scope {
        agent: bot.id,
        channel: h.main,
        epoch: 0,
    };
    let err = h
        .core
        .with_engine(move |e, now| {
            mcp::call_tool(
                e,
                now,
                scope,
                "chat_react",
                &json!({"message_id": m.id.0, "emoji": "lol"}),
            )
        })
        .await
        .unwrap()
        .unwrap_err();
    assert!(err.0.starts_with("invalid_emoji"), "{}", err.0);
    let _ = h.by(bot.id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompts_show_reactions_compactly() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [send_reply("On it.")]}, {"steps": []}]})),
        )
        .await;
    let note = h.say("the flaky test is back").await;
    h.core.react(h.hx.id, note.id, "💀", true).await.unwrap();
    let ping = h.say("@bot can you look").await;
    let prompts = h.wait_prompts("bot", 1).await;
    assert!(
        prompts[0].contains(&format!("the flaky test is back [💀 @hx]")),
        "reactions ride on the message line: {}",
        prompts[0]
    );
    assert!(
        prompts[0].contains("react with chat_react"),
        "the owed line mentions reacting: {}",
        prompts[0]
    );
    let reply = h.wait_message("reply", Some(bot.id)).await;
    settled(&mut h, 1).await;

    // After the turn: a reaction on the ping (already read) and one on the
    // bot's own reply. Neither wakes it; the next prompt carries both.
    h.advance(1_000).await;
    h.core.react(h.hx.id, ping.id, "👍", true).await.unwrap();
    h.core.react(h.hx.id, reply.id, "🎉", true).await.unwrap();
    h.advance(1_000).await;
    assert_eq!(h.prompts("bot").len(), 1, "reactions don't wake");
    h.say("@bot thanks").await;
    let prompts = h.wait_prompts("bot", 2).await;
    let p = &prompts[1];
    assert!(
        p.contains(&format!(
            "Reactions since you last read: 👍 @hx on #{}",
            ping.seq
        )),
        "older message's reaction gets its own line: {p}"
    );
    assert!(
        p.contains("On it. [🎉 @hx]"),
        "shown messages carry theirs inline: {p}"
    );
    assert!(
        !p.contains(&format!("🎉 @hx on #{}", reply.seq)),
        "not listed twice: {p}"
    );
}
