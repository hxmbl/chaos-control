//! Message editing: latest-own eligibility, revision history, mention re-derive,
//! and trigger behavior.

mod common;

use chaos_core::{engine::CoreEvent, store, types::*};
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_latest_own_composer_message_is_editable() {
    let h = Harness::new().await;
    let first = h.say("first").await;
    let second = h.say("second").await;

    // Not the latest.
    let err = h
        .core
        .edit_message(h.hx.id, h.main, first.id, "edited first")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not editable") || err.to_string().contains("NotEligible"));

    // Latest own composer message is editable.
    let edited = h
        .core
        .edit_message(h.hx.id, h.main, second.id, "edited second")
        .await
        .unwrap();
    assert_eq!(edited.id, second.id);
    assert_eq!(edited.seq, second.seq);
    assert_eq!(&*edited.body, "edited second");
    assert!(edited.edited_at.is_some());

    let history = h.history().await;
    assert_eq!(history.len(), 2);
    assert_eq!(&*history[0].body, "first");
    assert_eq!(&*history[1].body, "edited second");
    assert_eq!(history[1].edited_at, edited.edited_at);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_author_channel_and_non_composer_origins_are_rejected() {
    let mut h = Harness::new().await;
    let msg = h.say("hello").await;

    // Wrong author (another human).
    let other = h.core.human("other", "Other", h.main).await.unwrap();
    let err = h
        .core
        .edit_message(other.id, h.main, msg.id, "nope")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not the author") || err.to_string().contains("not editable"));

    // Wrong channel.
    let project = project_id();
    let other_channel = h.core.channel(&project, "side").await.unwrap();
    let err = h
        .core
        .edit_message(h.hx.id, other_channel.id, msg.id, "nope")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("no such message")
            || err.to_string().contains("not editable")
            || err.to_string().contains("not a member")
            || err.to_string().contains("unknown")
    );

    // Non-composer origin: agent/chat tool message.
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [send_reply("agent reply"), {"text": "done"}]}]})),
        )
        .await;
    h.say("@bot ping").await;
    let reply = h.wait_message("reply", Some(bot.id)).await;
    let err = h
        .core
        .edit_message(bot.id, h.main, reply.id, "edit")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not editable") || err.to_string().contains("NotEligible"));

    // Non-composer origin: system message.
    let sys = h
        .core
        .with_engine(move |e, now| {
            store::insert_message(
                e.conn(),
                &store::NewMessage {
                    channel: h.main,
                    author: None,
                    origin: Origin::System,
                    body: "notice",
                    reply_to: None,
                    mentions: &[],
                    hop: 0,
                    cause_batch: None,
                    client_request_id: None,
                    sent_at: now,
                },
            )
            .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap();
    let err = h
        .core
        .edit_message(h.hx.id, h.main, sys.id, "edit")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not editable") || err.to_string().contains("no such"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_or_whitespace_edit_is_rejected_without_revision() {
    let h = Harness::new().await;
    let msg = h.say("hello").await;

    for body in ["", "   ", "\n\t"] {
        let err = h
            .core
            .edit_message(h.hx.id, h.main, msg.id, body)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    let revs: i64 = h
        .core
        .with_engine(move |e, _| {
            e.conn()
                .query_row(
                    "SELECT COUNT(*) FROM message_revisions WHERE message_id = ?1",
                    [msg.id.0],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(revs, 0);
    let history = h.history().await;
    assert_eq!(&*history[0].body, "hello");
    assert_eq!(history[0].edited_at, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_edits_record_revisions_and_update_edited_at() {
    let h = Harness::new().await;
    let msg = h.say("v1").await;

    h.advance(1_000).await;
    let e1 = h
        .core
        .edit_message(h.hx.id, h.main, msg.id, "v2")
        .await
        .unwrap();
    h.advance(1_000).await;
    let e2 = h
        .core
        .edit_message(h.hx.id, h.main, msg.id, "v3")
        .await
        .unwrap();

    assert_eq!(e1.edited_at, Some(T0 + 1_000));
    assert_eq!(e2.edited_at, Some(T0 + 2_000));
    assert_eq!(&*e2.body, "v3");

    let revisions: Vec<(String, i64)> = h
        .core
        .with_engine(move |e, _| {
            let mut stmt = e
                .conn()
                .prepare(
                    "SELECT body, edited_at FROM message_revisions WHERE message_id = ?1 ORDER BY id",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([msg.id.0], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        revisions,
        vec![
            ("v1".to_string(), (T0 + 1_000) as i64),
            ("v2".to_string(), (T0 + 2_000) as i64)
        ]
    );

    let history = h.history().await;
    assert_eq!(&*history[0].body, "v3");
    assert_eq!(history[0].edited_at, Some(T0 + 2_000));
    assert_eq!(history[0].id, msg.id);
    assert_eq!(history[0].seq, msg.seq);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mentions_are_replaced_on_edit() {
    let mut h = Harness::new().await;
    let a = h.agent("alpha", script(json!({"turns": []}))).await;
    let b = h.agent("beta", script(json!({"turns": []}))).await;
    let msg = h.say("@alpha hi").await;

    assert_eq!(msg.mentions, vec![a.id]);

    let edited = h
        .core
        .edit_message(h.hx.id, h.main, msg.id, "@beta hi")
        .await
        .unwrap();
    assert_eq!(edited.mentions, vec![b.id]);

    let stored = h.history().await.pop().unwrap();
    assert_eq!(stored.mentions, vec![b.id]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_triggers_are_not_duplicated_and_new_mentions_can_wake_once() {
    let mut h = Harness::new().await;
    let alpha = h
        .agent(
            "alpha",
            script(json!({"turns": [
                {"steps": [{"text": "alpha ack"}]},
                {"steps": [{"text": "alpha should not wake again"}]}
            ]})),
        )
        .await;
    let beta = h
        .agent(
            "beta",
            script(json!({"turns": [{"steps": [{"text": "beta ack"}]}]})),
        )
        .await;

    let msg = h.say("@alpha ping").await;
    h.wait_message("alpha reply", Some(alpha.id)).await;
    h.until("alpha settled", move |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE agent_id = ?1 AND state = 'settled'",
            [alpha.id.0],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            >= 1
    })
    .await;
    let triggers_before = h.count("SELECT COUNT(*) FROM triggers").await;
    let alpha_prompts_before = h.prompts("alpha").len();

    let edited = h
        .core
        .edit_message(h.hx.id, h.main, msg.id, "@alpha @beta ping")
        .await
        .unwrap();
    assert!(edited.mentions.contains(&alpha.id));
    assert!(edited.mentions.contains(&beta.id));

    h.wait_message("beta reply", Some(beta.id)).await;
    h.until("beta settled", move |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE agent_id = ?1 AND state = 'settled'",
            [beta.id.0],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            >= 1
    })
    .await;

    // Alpha's existing trigger for this message must not be duplicated or re-wake.
    h.advance(60_000).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(h.prompts("alpha").len(), alpha_prompts_before);
    let alpha_triggers: i64 = h
        .core
        .with_engine(move |e, _| {
            e.conn()
                .query_row(
                    "SELECT COUNT(*) FROM triggers WHERE agent_id = ?1 AND message_id = ?2",
                    [alpha.id.0, msg.id.0],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(alpha_triggers, 1);

    let beta_triggers: i64 = h
        .core
        .with_engine(move |e, _| {
            e.conn()
                .query_row(
                    "SELECT COUNT(*) FROM triggers WHERE agent_id = ?1 AND message_id = ?2",
                    [beta.id.0, msg.id.0],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(beta_triggers, 1);
    assert_eq!(
        h.count("SELECT COUNT(*) FROM triggers").await,
        triggers_before + 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emits_message_edited_event() {
    let mut h = Harness::new().await;
    let msg = h.say("before").await;
    h.drain();

    let edited = h
        .core
        .edit_message(h.hx.id, h.main, msg.id, "after")
        .await
        .unwrap();

    let ev = h
        .wait_for(
            "edited event",
            |e| matches!(e, CoreEvent::MessageEdited(m) if m.id == edited.id),
        )
        .await;
    match ev {
        CoreEvent::MessageEdited(m) => {
            assert_eq!(&*m.body, "after");
            assert!(m.edited_at.is_some());
        }
        _ => unreachable!(),
    }
}
