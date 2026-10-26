//! The chat MCP server over real HTTP: scoping, tools, dedup, revocation and
//! the stdio bridge.

mod common;

use chaos_core::mcp;
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_requires_a_live_session_token() {
    let h = Harness::new().await;
    let url = h.core.mcp_url().to_string();
    let r = tokio::task::spawn_blocking(move || mcp::list_tools_http(&url, "not-a-token"))
        .await
        .unwrap();
    assert_eq!(r.unwrap_err(), "unauthorized");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn six_tools_scoped_to_the_session_and_revoked_on_close() {
    let mut h = Harness::new().await;
    // A real session: the fake agent lists the tools and reads before replying.
    let bot = h
        .agent("bot", script(json!({"turns": [{"steps": [
            {"mcp": {"tool": "chat_context", "args": {}}},
            {"mcp": {"tool": "chat_read", "args": {"limit": 5}}},
            {"mcp": {"tool": "chat_search", "args": {"query": "deploy"}}},
            {"mcp": {"tool": "chat_status", "args": {"text": "checking the deploy"}}},
            {"mcp": {"tool": "chat_send", "args": {"body": "@everyone deploy is fine", "reply_to": "$TRIGGER", "client_request_id": "x"}}},
            {"mcp": {"tool": "chat_send", "args": {"body": "spoofed?", "author": "hx", "client_request_id": "y"}}}
        ]}]})))
        .await;
    h.say("the deploy went out at 3pm").await;
    let ping = h.say("@bot did the deploy work?").await;
    let reply = h.wait_message("reply", Some(bot.id)).await;
    assert_eq!(reply.reply_to, Some(ping.id));
    let spoof = h.wait_message("second send", Some(bot.id)).await;
    assert_eq!(
        spoof.author,
        Some(bot.id),
        "author comes from the token, never the args"
    );
    h.quiesce().await;
    let results = h.fake("bot").observed.lock().unwrap().tool_results.clone();
    let ctx = results[0].as_ref().unwrap();
    assert_eq!(ctx["me"]["handle"], json!("bot"));
    assert_eq!(ctx["channel"]["name"], json!("main"));
    let read = results[1].as_ref().unwrap();
    assert!(read["messages"].as_array().unwrap().len() >= 2);
    let search = results[2].as_ref().unwrap();
    assert_eq!(search["hits"].as_array().unwrap().len(), 2);
    let send = results[4].as_ref().unwrap();
    assert!(
        send["notes"][0]
            .as_str()
            .unwrap()
            .contains("@everyone is not available")
    );
    assert_eq!(
        reply.mentions,
        Vec::new(),
        "@everyone from an agent mentions nobody"
    );

    // The token the fake got is the session's; after an idle close it is dead.
    let sessions = h.fake("bot").observed.lock().unwrap().sessions.clone();
    let token = sessions[0]["mcpServers"][0]["headers"][0]["value"]
        .as_str()
        .unwrap()
        .trim_start_matches("Bearer ")
        .to_string();
    let url = h.core.mcp_url().to_string();
    let (u, t) = (url.clone(), token.clone());
    let names = tokio::task::spawn_blocking(move || mcp::list_tools_http(&u, &t))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(names, mcp::TOOL_NAMES.map(String::from).to_vec());
    h.advance(10 * 60 * 1000 + 1).await;
    let r = tokio::task::spawn_blocking(move || mcp::list_tools_http(&url, &token))
        .await
        .unwrap();
    assert_eq!(r.unwrap_err(), "unauthorized", "revoked on close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_bridge_round_trips_json_rpc() {
    let mut h = Harness::new().await;
    let bot = h
        .agent(
            "bot",
            script(json!({"turns": [{"steps": [{"gate": "hold"}, {"text": "done"}]}]})),
        )
        .await;
    h.say("@bot hold on").await;
    h.until("session open", |c| {
        c.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))
            .unwrap()
            == 1
    })
    .await;
    let sessions = h.fake("bot").observed.lock().unwrap().sessions.clone();
    let token = sessions[0]["mcpServers"][0]["headers"][0]["value"]
        .as_str()
        .unwrap()
        .trim_start_matches("Bearer ")
        .to_string();
    let url = h.core.mcp_url().to_string();
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "bridge-test", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "chat_send", "arguments": {"body": "via the bridge", "client_request_id": "b1"}}}),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    let out = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        mcp::run_bridge(&url, &token, input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    })
    .await
    .unwrap();
    let lines: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        lines.len(),
        3,
        "one answer per request, none for the notification: {out}"
    );
    assert_eq!(lines[1]["result"]["tools"].as_array().unwrap().len(), 6);
    assert_eq!(lines[2]["result"]["isError"], json!(false));
    let m = h.wait_message("bridged post", Some(bot.id)).await;
    assert_eq!(&*m.body, "via the bridge");
    h.fake("bot").gates.open("hold");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_send_is_rate_limited_per_agent() {
    let mut h = Harness::new().await;
    let sends: Vec<_> = (0..7)
        .map(|i| json!({"mcp": {"tool": "chat_send", "args": {"body": format!("post {i}"), "client_request_id": format!("p{i}")}}}))
        .collect();
    let bot = h
        .agent("bot", script(json!({"turns": [{"steps": sends}]})))
        .await;
    h.say("@bot spam a bit").await;
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
    let results = h.fake("bot").observed.lock().unwrap().tool_results.clone();
    assert!(results[..6].iter().all(|r| r.is_ok()));
    assert!(
        results[6].as_ref().unwrap_err().contains("rate_limited"),
        "{:?}",
        results[6]
    );
    assert_eq!(h.by(bot.id).await.len(), 6);
}
