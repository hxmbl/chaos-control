//! Delivery scenarios through the real runtime with a scripted ACP agent.
//! Only the agent transport and the clock are fake (see `common`).

mod common;

use chaos_core::{engine::CoreEvent, types::*};
use common::*;
use serde_json::json;

fn agent_messages(msgs: &[Message], agent: MemberId) -> Vec<Message> {
    msgs.iter()
        .filter(|m| m.author == Some(agent))
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_mention_gets_exactly_one_reply() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                send_reply("on it: the flake is a timezone bug"),
                {"text": "I replied in the channel."}
            ]}]})),
        )
        .await;
    let ping = h.say("@bot why is CI red?").await;
    let reply = h.wait_message("bot's reply", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(ping.id));
    assert_eq!(reply.origin, Origin::ChatTool);
    h.until("batch settled", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'settled'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    })
    .await;
    h.quiesce().await;
    assert_eq!(h.prompts("bot").len(), 1, "one delivery");
    assert_eq!(
        agent_messages(&h.history().await, bot.id).len(),
        1,
        "one reply, no fallback"
    );
    assert_eq!(
        h.obligation(bot.id, ping.id).await,
        Some(ObligationState::Satisfied)
    );
    let prompt = &h.prompts("bot")[0];
    assert!(
        prompt.contains("You are @bot"),
        "fresh session gets the room norms"
    );
    assert!(prompt.contains(&format!("You owe a reply to id:{}", ping.id)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_trigger_causes_no_second_delivery() {
    let mut h = Harness::new().await;
    // The agent retries its own send with the same request id: deduped.
    let bot = h
        .agent("bot", script(json!({"turns": [{"steps": [
            {"mcp": {"tool": "chat_send", "args": {"body": "answer", "reply_to": "$TRIGGER", "client_request_id": "same"}}},
            {"mcp": {"tool": "chat_send", "args": {"body": "answer", "reply_to": "$TRIGGER", "client_request_id": "same"}}}
        ]}]})))
        .await;
    let ping = h.say("@bot @bot hello (mentioned twice)").await;
    h.wait_message("reply", Some(bot.id)).await;
    h.until("settled", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'settled'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    })
    .await;
    // Re-running trigger creation for the same message (e.g. startup
    // reconciliation) is a no-op.
    h.core.retrigger(ping.id).await.unwrap();
    h.core.retrigger(ping.id).await.unwrap();
    h.advance(60_000).await;
    assert_eq!(h.prompts("bot").len(), 1);
    assert_eq!(h.count("SELECT COUNT(*) FROM triggers").await, 1);
    assert_eq!(h.count("SELECT COUNT(*) FROM obligations").await, 1);
    assert_eq!(h.count("SELECT COUNT(*) FROM batches").await, 1);
    assert_eq!(agent_messages(&h.history().await, bot.id).len(), 1);
    let results = h.fake("bot").observed.lock().unwrap().tool_results.clone();
    assert_eq!(results.len(), 2);
    assert_eq!(results[1].as_ref().unwrap()["deduped"], json!(true));
    assert_eq!(
        results[0].as_ref().unwrap()["message_id"],
        results[1].as_ref().unwrap()["message_id"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_pings_mid_turn_merge_into_one_batch_and_keep_both_obligations() {
    let mut h = Harness::new().await;
    let bot = h
        .agent("bot", script(json!({"turns": [
            {"steps": [{"tool_call": "Reading the logs"}, {"gate": "first"}, {"text": "First answer."}]},
            {"steps": [{"text": "Answers to both of you."}]}
        ]})))
        .await;
    let one = h.say("@bot first question").await;
    h.until("first turn accepted", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'accepted'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    })
    .await;
    let two = h.say("@bot second question").await;
    let three = h.say_reply("and a third", one.id).await; // reply to a message by... a human; mention below
    let four = h.say("@bot third question").await;
    let main = h.main;
    let bot_id = bot.id;
    // One pending batch holding both new triggers, each with its obligation.
    h.until("pending batch with two triggers", move |c| {
        let p = chaos_core::store::pending_batch(c, bot_id, main).unwrap();
        p.is_some_and(|p| chaos_core::store::batch_triggers(c, p.id).unwrap().len() == 2)
    })
    .await;
    assert_eq!(
        h.count("SELECT COUNT(*) FROM batches WHERE state = 'pending'")
            .await,
        1
    );
    assert_eq!(
        h.obligation(bot.id, two.id).await,
        Some(ObligationState::Open)
    );
    assert_eq!(
        h.obligation(bot.id, four.id).await,
        Some(ObligationState::Open)
    );
    assert_eq!(
        h.obligation(bot.id, three.id).await,
        None,
        "untargeted: no trigger"
    );
    h.fake("bot").gates.open("first");
    let first = h
        .wait_message("fallback to the first ping", Some(bot.id))
        .await;
    assert_eq!(first.reply_to, Some(one.id));
    assert_eq!(first.origin, Origin::Fallback);
    let second = h
        .wait_message("fallback to the merged batch", Some(bot.id))
        .await;
    assert_eq!(second.origin, Origin::Fallback);
    assert_eq!(
        second.reply_to,
        Some(four.id),
        "one fallback, to the newest open trigger"
    );
    h.quiesce().await;
    let prompts = h.prompts("bot");
    assert_eq!(
        prompts.len(),
        2,
        "the two mid-turn pings were delivered as one turn"
    );
    assert!(
        prompts[1].contains(&format!("id:{}", two.id))
            && prompts[1].contains(&format!("id:{}", four.id))
    );
    assert_eq!(
        h.obligation(bot.id, two.id).await,
        Some(ObligationState::Fallback)
    );
    assert_eq!(
        h.obligation(bot.id, four.id).await,
        Some(ObligationState::Fallback)
    );
    assert_eq!(agent_messages(&h.history().await, bot.id).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_reply_only_when_the_obligation_is_unmet() {
    let mut h = Harness::new().await;
    let quiet = h
        .agent(
            "quiet",
            script(json!({"turns": [{"steps": [{"text": "The answer is 42."}]}]})),
        )
        .await;
    let ping = h.say("@quiet what is the answer?").await;
    let m = h.wait_message("fallback", Some(quiet.id)).await;
    assert_eq!(m.origin, Origin::Fallback);
    assert_eq!(m.reply_to, Some(ping.id));
    assert_eq!(&*m.body, "The answer is 42.");
    assert_eq!(
        h.obligation(quiet.id, ping.id).await,
        Some(ObligationState::Fallback)
    );

    // A reaction that answers it: no fallback.
    let reactor = h
        .agent(
            "reactor",
            script(json!({"turns": [{"steps": [
                {"mcp": {"tool": "chat_react", "args": {"message_id": "$TRIGGER", "emoji": "✅"}}},
                {"text": "Done, reacted."}
            ]}]})),
        )
        .await;
    let ping2 = h.say("@reactor please ack").await;
    h.wait_for(
        "reaction",
        |e| matches!(e, CoreEvent::Reaction { member, .. } if *member == reactor.id),
    )
    .await;
    h.until("settled", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'settled'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 2
    })
    .await;
    assert_eq!(
        h.obligation(reactor.id, ping2.id).await,
        Some(ObligationState::Satisfied)
    );
    assert!(agent_messages(&h.history().await, reactor.id).is_empty());

    // An "on it" reaction does not answer: the fallback still posts.
    let eyes = h
        .agent(
            "eyes",
            script(json!({"turns": [{"steps": [
                {"mcp": {"tool": "chat_react", "args": {"message_id": "$TRIGGER", "emoji": "👀"}}},
                {"text": "Here is the actual answer."}
            ]}]})),
        )
        .await;
    let ping3 = h.say("@eyes look at this").await;
    let m = h.wait_message("fallback after 👀", Some(eyes.id)).await;
    assert_eq!(m.reply_to, Some(ping3.id));

    // Turn ends with no text and no reply: unmet, nothing posted.
    let mute = h
        .agent("mute", script(json!({"turns": [{"steps": []}]})))
        .await;
    let ping4 = h.say("@mute hello?").await;
    h.until("mute settled", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'settled'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 4
    })
    .await;
    assert_eq!(
        h.obligation(mute.id, ping4.id).await,
        Some(ObligationState::Unmet)
    );
    assert!(agent_messages(&h.history().await, mute.id).is_empty());
    let st = h.core.status(mute.id, h.main).await.unwrap();
    assert_eq!(st.presence.as_deref(), Some("finished without replying"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_never_duplicates_an_explicit_send() {
    let mut h = Harness::new().await;
    // No reply_to, but it is the only open obligation: the send answers it.
    // The final text repeats the sent text.
    let bot = h
        .agent("bot", script(json!({"turns": [
            {"steps": [
                {"mcp": {"tool": "chat_send", "args": {"body": "Fixed in abc123.", "client_request_id": "a"}}},
                {"text": "Fixed in abc123."}
            ]},
            {"steps": [
                send_reply("Short answer: yes."),
                {"text": "I told them yes, and here is a longer private summary."}
            ]}
        ]})))
        .await;
    let p1 = h.say("@bot is it fixed?").await;
    h.wait_message("explicit send", Some(bot.id)).await;
    h.until("first settled", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'settled'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    })
    .await;
    let p2 = h.say("@bot can we ship?").await;
    h.wait_message("second explicit send", Some(bot.id)).await;
    h.until("second settled", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'settled'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 2
    })
    .await;
    h.quiesce().await;
    let mine = agent_messages(&h.history().await, bot.id);
    assert_eq!(
        mine.len(),
        2,
        "no fallback after either explicit send: {mine:#?}"
    );
    assert!(mine.iter().all(|m| m.origin == Origin::ChatTool));
    assert_eq!(
        h.obligation(bot.id, p1.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(
        h.obligation(bot.id, p2.id).await,
        Some(ObligationState::Satisfied)
    );
    assert_eq!(
        h.count("SELECT COUNT(*) FROM messages WHERE origin = 'fallback'")
            .await,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_ping_pong_damper_trips() {
    let mut h = Harness::new().await;
    let turns = |me: &str, other: &str| {
        let t: Vec<_> = (0..6)
            .map(|i| json!({"steps": [
                {"gate": format!("{me}{i}")},
                {"mcp": {"tool": "chat_send", "args": {"body": format!("@{other} over to you ({i})"), "client_request_id": "r$N"}}}
            ]}))
            .collect();
        script(json!({"turns": t}))
    };
    let a = h.agent("a", turns("a", "b")).await;
    let b = h.agent("b", turns("b", "a")).await;
    h.say("@a start the relay").await; // human addresses a only
    let mut a_turn = 0;
    let mut b_turn = 0;
    // Alternate: a posts, a human chats (resets hops, not the damper), b posts, ...
    let mut tripped = None;
    for round in 0..6 {
        let (fake, gate, who) = if round % 2 == 0 {
            a_turn += 1;
            ("a", format!("a{}", a_turn - 1), a.id)
        } else {
            b_turn += 1;
            ("b", format!("b{}", b_turn - 1), b.id)
        };
        h.fake(fake).gates.open(&gate);
        h.wait_message("relay post", Some(who)).await;
        h.say("unrelated human chatter").await;
        h.quiesce().await;
        h.drain();
        if let Some(CoreEvent::Message(m)) = h
            .seen
            .iter()
            .find(|e| matches!(e, CoreEvent::Message(m) if m.origin == Origin::System))
            .cloned()
        {
            tripped = Some((round, m));
            break;
        }
    }
    let (round, notice) = tripped.expect("the damper tripped");
    assert!(notice.body.contains("Pausing @a ↔ @b"), "{}", notice.body);
    assert_eq!(
        round, 4,
        "four alternating wakes allowed, the fifth is damped"
    );
    // Every agent post had hop 1 (each followed human chatter), so this was
    // the damper, not the hop budget.
    for m in h
        .history()
        .await
        .iter()
        .filter(|m| m.origin == Origin::ChatTool)
    {
        assert_eq!(m.hop, 1);
    }
    // The paused pair stays paused: b got no wake from a's last post.
    let b_prompts = h.prompts("b").len();
    h.quiesce().await;
    assert_eq!(h.prompts("b").len(), b_prompts);
    // A human addressing either one lifts the pause.
    h.say("@b carry on please").await;
    h.until("b woken by the human", move |c| {
        c.query_row(
            "SELECT COUNT(*) FROM triggers WHERE agent_id = ?1",
            [b.id.0],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            >= 3
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hop_budget_stops_agent_chains_and_a_human_message_resets_it() {
    let mut h = Harness::new().await;
    let relay = |other: &str| {
        script(json!({"turns": [{"steps": [
            {"mcp": {"tool": "chat_send", "args": {"body": format!("@{other} your turn"), "client_request_id": "r$N"}}}
        ]}]}))
    };
    let a = h.agent("a", relay("b")).await;
    let _b = h.agent("b", relay("a")).await;
    h.say("@a kick off").await;
    // a(1) -> b(2) -> a(3) -> b(4): the hop-4 post wakes nobody.
    h.until("chain ended", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM messages WHERE origin = 'chat_tool'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 4
    })
    .await;
    h.quiesce().await;
    let hops: Vec<u8> = h
        .history()
        .await
        .iter()
        .filter(|m| m.origin == Origin::ChatTool)
        .map(|m| m.hop)
        .collect();
    assert_eq!(hops, vec![1, 2, 3, 4]);
    assert_eq!(h.prompts("a").len(), 2);
    assert_eq!(h.prompts("b").len(), 2);
    let st = h.core.status(a.id, h.main).await.unwrap();
    assert!(st.presence.unwrap().contains("hop limit"));

    // Same relay, but a human speaks before b's hop-3 post goes out.
    let mut h = Harness::new().await;
    let a = h.agent("a", relay("b")).await;
    let b = h
        .agent("b", script(json!({"turns": [
            {"steps": [{"mcp": {"tool": "chat_send", "args": {"body": "@a your turn", "client_request_id": "r$N"}}}]},
            {"steps": [{"gate": "late"}, {"mcp": {"tool": "chat_send", "args": {"body": "@a your turn", "client_request_id": "r$N"}}}]}
        ]})))
        .await;
    h.say("@a kick off").await;
    h.until("b's second turn running", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM messages WHERE origin = 'chat_tool'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 3
    })
    .await;
    h.say("nice, keep going").await; // untargeted: wakes nobody, resets hops
    h.fake("b").gates.open("late");
    h.until("a woken a third time", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM messages WHERE origin = 'chat_tool'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            >= 5
    })
    .await;
    let b_posts = h.by(b.id).await;
    assert_eq!(b_posts[1].hop, 1, "reset by the human message");
    assert!(h.prompts("a").len() >= 3);
    let _ = a;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_v1_turn_is_never_retried_after_acceptance() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                {"tool_call": "Running the full test suite"},
                {"gate": "done"},
                {"text": "All 4,000 tests pass."}
            ]}]})),
        )
        .await;
    let ping = h.say("@bot run the tests").await;
    h.until("accepted", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'accepted'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    })
    .await;
    let st = h.core.status(bot.id, h.main).await.unwrap();
    assert_eq!(st.glow, Glow::Active);
    assert_eq!(
        st.presence.as_deref(),
        Some("working: Running the full test suite")
    );
    // Two hours of fake time pass in big steps; nothing may re-send.
    for _ in 0..12 {
        h.advance(10 * 60 * 1000).await;
    }
    assert_eq!(h.prompts("bot").len(), 1);
    assert_eq!(h.count("SELECT attempts FROM batches").await, 1);
    h.fake("bot").gates.open("done");
    let reply = h.wait_message("reply", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(ping.id));
    assert_eq!(&*reply.body, "All 4,000 tests pass.");
    h.quiesce().await;
    assert_eq!(h.prompts("bot").len(), 1);

    // Slow *before* acceptance (no update at all yet) is not a failure either:
    // it is never retried while the request is outstanding.
    let mut h = Harness::new().await;
    let _slow = h
        .agent(
            "slow",
            script(json!({"turns": [{"steps": [{"gate": "go"}, {"text": "ok"}]}]})),
        )
        .await;
    h.say("@slow hi").await;
    h.until("dispatched", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state = 'dispatched'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    })
    .await;
    for _ in 0..6 {
        h.advance(60_000).await;
    }
    assert_eq!(h.prompts("slow").len(), 1);
    h.fake("slow").gates.open("go");
    h.wait_message("reply", Some(_slow.id)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_request_is_declined_with_the_agents_own_option() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [
                {"permission": "rm -rf build/"},
                {"text": "I wasn't allowed to delete build/."}
            ]}]})),
        )
        .await;
    let ping = h.say("@bot clean the build dir").await;
    let note = h.wait_message("system note", None).await;
    assert!(
        note.body.contains("@bot asked to run rm -rf build/"),
        "{}",
        note.body
    );
    assert!(
        note.body.contains("declined with its own \"Deny\" option"),
        "{}",
        note.body
    );
    assert!(note.body.contains("woken by @hx"), "{}", note.body);
    let reply = h.wait_message("fallback", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(ping.id));
    let outcomes = h
        .fake("bot")
        .observed
        .lock()
        .unwrap()
        .permission_outcomes
        .clone();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["outcome"]["outcome"], json!("selected"));
    assert_eq!(
        outcomes[0]["outcome"]["optionId"],
        json!("deny"),
        "never the allow option"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_crash_and_restart_goes_red_after_three_failures() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [{"exit": null}]}]})),
        )
        .await;
    h.say("@bot are you there?").await;
    h.until("first failure", |c| {
        c.query_row("SELECT attempts FROM batches", [], |r| r.get::<_, i64>(0))
            .unwrap()
            == 1
            && c.query_row("SELECT state FROM batches", [], |r| r.get::<_, String>(0))
                .unwrap()
                == "pending"
    })
    .await;
    assert_eq!(
        h.core.status(bot.id, h.main).await.unwrap().glow,
        Glow::Active,
        "retrying"
    );
    h.advance(1_000).await; // backoff 1s
    h.until("second failure", |c| {
        c.query_row("SELECT attempts FROM batches", [], |r| r.get::<_, i64>(0))
            .unwrap()
            == 2
            && c.query_row("SELECT state FROM batches", [], |r| r.get::<_, String>(0))
                .unwrap()
                == "pending"
    })
    .await;
    h.advance(5_000).await; // backoff 5s
    h.until("third failure", |c| {
        c.query_row("SELECT attempts FROM batches", [], |r| r.get::<_, i64>(0))
            .unwrap()
            == 3
            && c.query_row("SELECT state FROM batches", [], |r| r.get::<_, String>(0))
                .unwrap()
                == "pending"
    })
    .await;
    h.quiesce().await;
    let st = h.core.status(bot.id, h.main).await.unwrap();
    assert_eq!(st.glow, Glow::Unresponsive);
    assert!(st.presence.unwrap().contains("3 failed attempts"));
    h.advance(10 * 60 * 1000).await;
    assert_eq!(h.prompts("bot").len(), 3, "no automatic retries once red");
    // A human ping is a fresh start.
    h.say("@bot try again").await;
    h.until("fourth attempt", |c| {
        c.query_row("SELECT attempts FROM batches", [], |r| r.get::<_, i64>(0))
            .unwrap()
            == 4
    })
    .await;
    assert_eq!(h.wait_prompts("bot", 4).await.len(), 4);
    assert_eq!(
        h.count(
            "SELECT COUNT(*) FROM messages WHERE author_id IS NOT NULL AND origin != 'composer'"
        )
        .await,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untargeted_message_wakes_nobody() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [{"text": "hi"}]}]})),
        )
        .await;
    h.say("morning all, CI is green again").await;
    h.say("mail me at someone@bot.example").await;
    h.advance(60_000).await;
    assert!(h.prompts("bot").is_empty());
    assert_eq!(h.count("SELECT COUNT(*) FROM triggers").await, 0);
    assert_eq!(h.count("SELECT COUNT(*) FROM batches").await, 0);
    assert_eq!(
        h.core.status(bot.id, h.main).await.unwrap().glow,
        Glow::Sleeping
    );
    assert_eq!(h.core.live_tokens(), 0, "no session was started");
    // When it is woken later, the untargeted messages arrive as context.
    h.say("@bot anything new?").await;
    h.wait_message("reply", Some(bot.id)).await;
    assert!(h.prompts("bot")[0].contains("morning all, CI is green again"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_negotiation_falls_back_to_v1_and_caps_are_honest() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [send_reply("hello")]}]})),
        )
        .await;
    h.say("@bot hi").await;
    h.wait_message("reply", Some(bot.id)).await;
    let adapter = h.agents["bot"].2.clone();
    assert_eq!(
        adapter.negotiated_version(),
        Some(1),
        "v1-only agent: v2 draft offered, v1 selected"
    );
    let sessions = h.fake("bot").observed.lock().unwrap().sessions.clone();
    assert_eq!(sessions.len(), 1);
    let servers = sessions[0]["mcpServers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(
        servers[0]["type"],
        json!("http"),
        "agent advertises HTTP MCP"
    );
    assert_eq!(servers[0]["name"], json!("chaos-control"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_session_closes_and_its_token_is_revoked() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [send_reply("hello")]}]})),
        )
        .await;
    h.say("@bot hi").await;
    h.wait_message("reply", Some(bot.id)).await;
    h.quiesce().await;
    assert_eq!(h.core.live_tokens(), 1);
    assert_eq!(
        h.core.status(bot.id, h.main).await.unwrap().glow,
        Glow::Active
    );
    h.advance(10 * 60 * 1000 + 1).await;
    h.quiesce().await;
    assert_eq!(h.core.live_tokens(), 0, "token revoked on close");
    assert_eq!(
        h.core.status(bot.id, h.main).await.unwrap().glow,
        Glow::Sleeping
    );
}
