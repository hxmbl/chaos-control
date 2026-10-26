//! The key proof: one trigger creates at most one reply obligation and at most
//! one reply, whatever mix of explicit sends, reactions, fallbacks, merges and
//! duplicate retriggers happens. Seeded pseudo-random runs through the real
//! runtime (fake transport and clock only).

mod common;

use chaos_core::types::*;
use common::*;
use serde_json::{Value, json};

/// Tiny deterministic generator (no extra dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

fn random_turn(rng: &mut Lcg, i: usize) -> Value {
    let mut steps = vec![json!({"tool_call": "thinking"})];
    match rng.next(5) {
        0 => steps.push(send_reply("explicit reply")),
        1 => steps.push(json!({"mcp": {"tool": "chat_send", "args": {"body": "unthreaded reply", "client_request_id": format!("u{i}")}}})),
        2 => steps.push(json!({"mcp": {"tool": "chat_react", "args": {"message_id": "$TRIGGER", "emoji": "✅"}}})),
        3 => {
            // A retried send (same request id): must dedup.
            steps.push(send_reply("retried reply"));
            steps.push(send_reply("retried reply"));
        }
        _ => {}
    }
    if rng.next(2) == 0 {
        steps.push(json!({"gate": format!("g{i}")}));
    }
    match rng.next(3) {
        0 => {}
        1 => steps.push(json!({"text": "explicit reply"})), // same as a sent body
        _ => steps.push(json!({"text": format!("final text {i}")})),
    }
    json!({"steps": steps})
}

async fn run(seed: u64) {
    let mut rng = Lcg(seed);
    let mut h = Harness::new().await;
    let turns: Vec<Value> = (0..12).map(|i| random_turn(&mut rng, i)).collect();
    let bot = h.agent("bot", script(json!({"turns": turns}))).await;
    let mut pings = Vec::new();
    for i in 0..8 {
        let m = h.say(&format!("@bot question {i}")).await;
        pings.push(m.id);
        if rng.next(3) == 0 {
            h.core.retrigger(m.id).await.unwrap();
        }
        if rng.next(2) == 0 {
            h.quiesce().await;
        }
        for g in 0..12 {
            if rng.next(3) == 0 {
                h.fake("bot").gates.open(&format!("g{g}"));
            }
        }
    }
    for g in 0..12 {
        h.fake("bot").gates.open(&format!("g{g}"));
    }
    h.until("all batches done", |c| {
        c.query_row(
            "SELECT COUNT(*) FROM batches WHERE state IN ('pending', 'dispatched', 'accepted')",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 0
    })
    .await;
    h.quiesce().await;

    let history = h.history().await;
    for ping in &pings {
        let obligations = h
            .count_param(
                "SELECT COUNT(*) FROM obligations WHERE message_id = ?1",
                ping.0,
            )
            .await;
        assert!(
            obligations <= 1,
            "seed {seed}: ping {ping} has {obligations} obligations"
        );
        let replies: Vec<&Message> = history
            .iter()
            .filter(|m| m.author == Some(bot.id) && m.reply_to == Some(*ping))
            .collect();
        assert!(
            replies.len() <= 1,
            "seed {seed}: ping {ping} got {} replies: {replies:#?}",
            replies.len()
        );
        let state = h.obligation(bot.id, *ping).await;
        assert_ne!(
            state,
            Some(ObligationState::Open),
            "seed {seed}: obligation left open"
        );
        if state == Some(ObligationState::Fallback) {
            // Answered by the batch's single fallback message, which replies to
            // the newest open trigger of that batch (maybe this one).
            let by = h
                .count_param(
                    "SELECT COALESCE(resolved_by, 0) FROM obligations WHERE message_id = ?1",
                    ping.0,
                )
                .await;
            let fallback = history
                .iter()
                .find(|m| m.id.0 == by)
                .expect("resolving message exists");
            assert_eq!(fallback.origin, Origin::Fallback, "seed {seed}");
            assert_eq!(fallback.author, Some(bot.id), "seed {seed}");
        }
    }
    // One delivery per batch, never a re-send after acceptance.
    let batches = h
        .count("SELECT COUNT(*) FROM batches WHERE state = 'settled' AND attempts = 1")
        .await;
    let all = h.count("SELECT COUNT(*) FROM batches").await;
    assert_eq!(
        batches, all,
        "seed {seed}: every batch delivered exactly once"
    );
    assert_eq!(
        h.prompts("bot").len() as i64,
        all,
        "seed {seed}: prompts == batches"
    );
}

impl Harness {
    async fn count_param(&self, sql: &'static str, p: i64) -> i64 {
        self.core
            .with_engine(move |e, _| e.conn().query_row(sql, [p], |r| r.get(0)).unwrap())
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_trigger_at_most_one_obligation_and_one_reply() {
    for seed in [1, 7, 42, 1234, 9001, 31337] {
        run(seed).await;
    }
}
