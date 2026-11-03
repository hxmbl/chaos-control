//! Emoji reactions (STRM-467): add and remove are idempotent, a reaction
//! never wakes anyone (not even the message's author), a pinged agent's
//! reaction with an answering emoji (✅/👍 by default; per agent, then per
//! project, configurable; never 👀) closes its obligation as handled, and agents
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
async fn by_default_check_and_thumbs_up_answer_a_ping_and_other_emoji_dont() {
    let mut h = Harness::new().await;
    // ✅ and nothing else: handled, not ignored, and nothing posted.
    let check = h
        .agent(
            "check",
            script(json!({"turns": [{"steps": [react(":white_check_mark:")]}]})),
        )
        .await;
    let ping = h.say("@check did CI pass?").await;
    h.wait_for(
        "check reaction",
        |e| matches!(e, CoreEvent::Reaction { member, added: true, .. } if *member == check.id),
    )
    .await;
    settled(&mut h, 1).await;
    assert_eq!(
        h.obligation(check.id, ping.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(resolved_kind(&h, ping.id).await.as_deref(), Some("react"));
    assert!(h.by(check.id).await.is_empty(), "a reaction posts nothing");
    let st = h.core.status(check.id, h.main).await.unwrap();
    assert_ne!(
        st.presence.as_deref(),
        Some("read it and let it go"),
        "reacted is not ignored"
    );
    // The prompt named the answering emoji.
    assert!(
        h.prompts("check")[0].contains("✅ or 👍 answers it"),
        "{}",
        h.prompts("check")[0]
    );

    // 💀 is recorded but doesn't answer: the obligation stays open during the
    // turn and the usual turn-end rules apply. With final text, that text is
    // the fallback reply; with none, the ping was ignored.
    let skull = h
        .agent(
            "skull",
            script(json!({"turns": [{"steps": [react("💀"), {"text": "Ouch."}]}]})),
        )
        .await;
    let ping2 = h.say("@skull I broke CI again").await;
    settled(&mut h, 2).await;
    assert_eq!(
        h.obligation(skull.id, ping2.id).await,
        Some(ObligationState::Fallback)
    );
    assert_eq!(h.by(skull.id).await.len(), 1, "the final text was posted");
    assert_eq!(
        h.count("SELECT COUNT(*) FROM reactions WHERE emoji = '💀'")
            .await,
        1
    );

    let quiet = h
        .agent(
            "quiet",
            script(json!({"turns": [{"steps": [react("💀")]}]})),
        )
        .await;
    let ping3 = h.say("@quiet I broke it again").await;
    settled(&mut h, 3).await;
    assert_eq!(
        h.obligation(quiet.id, ping3.id).await,
        Some(ObligationState::Ignored)
    );

    // 👀 ("on it") holds the obligation during the turn; with no final text
    // the turn still counts as handled, not ignored.
    let eyes = h
        .agent(
            "eyes",
            script(json!({"turns": [{"steps": [react(":eyes:")]}]})),
        )
        .await;
    let ping4 = h.say("@eyes can you look").await;
    settled(&mut h, 4).await;
    assert_eq!(
        h.obligation(eyes.id, ping4.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(resolved_kind(&h, ping4.id).await.as_deref(), Some("react"));

    // No reaction and no text is still `ignored`.
    let mute = h
        .agent("mute", script(json!({"turns": [{"steps": []}]})))
        .await;
    let ping5 = h.say("@mute hello?").await;
    settled(&mut h, 5).await;
    assert_eq!(
        h.obligation(mute.id, ping5.id).await,
        Some(ObligationState::Ignored)
    );

    // Taking the reaction back later doesn't reopen anything.
    h.core.react(check.id, ping.id, "✅", false).await.unwrap();
    assert_eq!(
        h.obligation(check.id, ping.id).await,
        Some(ObligationState::Satisfied)
    );
    h.advance(10 * 60 * 1000).await;
    assert_eq!(h.prompts("check").len(), 1, "never retried");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agents_own_list_wins_over_the_project_setting() {
    let mut h = Harness::new().await;
    // The project says only 🚀 answers; @own says only 💀 does.
    let set = h
        .core
        .set_ping_answer_reactions(project_id(), Some(vec![":rocket:".into()]))
        .await
        .unwrap();
    assert_eq!(set, vec!["🚀"]);
    assert_eq!(
        h.core.ping_answer_reactions(project_id()).await.unwrap(),
        Some(vec!["🚀".to_string()])
    );
    let own = h
        .agent_answering(
            "own",
            script(json!({"turns": [{"steps": [react("💀")]}, {"steps": [react("🚀")]}]})),
            &["💀"],
        )
        .await;
    let proj = h
        .agent(
            "proj",
            script(json!({"turns": [{"steps": [react("🚀")]}, {"steps": [react("✅")]}]})),
        )
        .await;
    let p1 = h.say("@own @proj ship?").await;
    settled(&mut h, 2).await;
    assert_eq!(
        h.obligation(own.id, p1.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(
        h.obligation(proj.id, p1.id).await,
        Some(ObligationState::Satisfied)
    );
    // The other way round: 🚀 doesn't answer for @own, ✅ (the default) not
    // for @proj while the project setting is in force.
    let p2 = h.say("@own @proj and now?").await;
    settled(&mut h, 4).await;
    assert_eq!(
        h.obligation(own.id, p2.id).await,
        Some(ObligationState::Ignored)
    );
    assert_eq!(
        h.obligation(proj.id, p2.id).await,
        Some(ObligationState::Ignored)
    );
    // chat_context tells each agent its own list.
    let ctx = tool(&h, own.id, "chat_context", json!({})).await;
    assert_eq!(ctx["ping_answer_reactions"], json!(["💀"]));
    let ctx = tool(&h, proj.id, "chat_context", json!({})).await;
    assert_eq!(ctx["ping_answer_reactions"], json!(["🚀"]));

    // Clearing the project setting goes back to the default.
    let cleared = h
        .core
        .set_ping_answer_reactions(project_id(), None)
        .await
        .unwrap();
    assert_eq!(cleared, vec!["✅", "👍"]);
    assert_eq!(
        h.core.ping_answer_reactions(project_id()).await.unwrap(),
        None
    );
    let ctx = tool(&h, proj.id, "chat_context", json!({})).await;
    assert_eq!(ctx["ping_answer_reactions"], json!(["✅", "👍"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wildcard_lets_any_emoji_but_eyes_answer() {
    let mut h = Harness::new().await;
    h.core
        .set_ping_answer_reactions(project_id(), Some(vec!["*".into()]))
        .await
        .unwrap();
    let skull = h
        .agent(
            "skull",
            script(json!({"turns": [{"steps": [react("💀")]}]})),
        )
        .await;
    let ping = h.say("@skull I broke CI again").await;
    settled(&mut h, 1).await;
    assert_eq!(
        h.obligation(skull.id, ping.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(resolved_kind(&h, ping.id).await.as_deref(), Some("react"));
    assert!(
        h.prompts("skull")[0].contains("any emoji but 👀 answers it"),
        "{}",
        h.prompts("skull")[0]
    );
    // chat_react says whether it answered.
    let other = h.say("another").await;
    let r = tool(
        &h,
        skull.id,
        "chat_react",
        json!({"message_id": other.id.0, "emoji": "🎉"}),
    )
    .await;
    assert_eq!(r["answers_pings"], json!(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eyes_always_keeps_the_obligation_open_during_the_turn() {
    let mut h = Harness::new().await;
    // Even when every emoji answers (project "*" and the agent's own "*"),
    // 👀 leaves the reply owed: the final text is still posted as the reply.
    h.core
        .set_ping_answer_reactions(project_id(), Some(vec!["*".into()]))
        .await
        .unwrap();
    let eyes = h
        .agent_answering(
            "eyes",
            script(json!({"turns": [{"steps": [react("👀"), {"text": "Looking now."}]}]})),
            &["*"],
        )
        .await;
    let ping = h.say("@eyes can you look").await;
    let reply = h.wait_message("eyes' fallback", Some(eyes.id)).await;
    settled(&mut h, 1).await;
    assert_eq!(&*reply.body, "Looking now.");
    assert_eq!(
        h.obligation(eyes.id, ping.id).await,
        Some(ObligationState::Fallback)
    );
    let other = h.say("another").await;
    let r = tool(
        &h,
        eyes.id,
        "chat_react",
        json!({"message_id": other.id.0, "emoji": ":eyes:"}),
    )
    .await;
    assert_eq!(r["answers_pings"], json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_list_is_rejected_whole_with_every_bad_entry_named() {
    let h = Harness::new().await;
    h.core
        .set_ping_answer_reactions(project_id(), Some(vec!["👍".into()]))
        .await
        .unwrap();
    let err = h
        .core
        .set_ping_answer_reactions(
            project_id(),
            Some(vec!["✅".into(), "lol".into(), "👀".into(), "".into()]),
        )
        .await
        .unwrap_err();
    assert!(err.contains("invalid ping_answer_reactions"), "{err}");
    assert!(err.contains("\"lol\""), "{err}");
    assert!(err.contains("\"👀\""), "{err}");
    assert!(err.contains("never answers a ping"), "{err}");
    // Nothing was written: the old setting stands.
    assert_eq!(
        h.core.ping_answer_reactions(project_id()).await.unwrap(),
        Some(vec!["👍".to_string()])
    );
    // A project core doesn't know is refused too.
    let unknown = ProjectId::parse("11111111-2222-4333-8444-555555555555").unwrap();
    let err = h
        .core
        .set_ping_answer_reactions(unknown, Some(vec!["✅".into()]))
        .await
        .unwrap_err();
    assert_eq!(err, "no such project");
    // A stored value that no longer parses (hand-edited) counts as unset.
    h.core
        .with_engine(|e, _| {
            e.conn()
                .execute(
                    "UPDATE project_settings SET value = '[\"nope\"]' WHERE key = 'ping_answer_reactions'",
                    [],
                )
                .unwrap();
        })
        .await
        .unwrap();
    assert_eq!(
        h.core.ping_answer_reactions(project_id()).await.unwrap(),
        None
    );
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
        prompts[0].contains("the flaky test is back [💀 @hx]"),
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
