//! Recorded ACP transcripts replayed through the real adapter, plus the fake
//! agent binary over a real process and stdio.
//!
//! Fixtures are NDJSON `{dir, frame}` lines (`out` = client to agent, `in` =
//! agent to client), the format `SpawnTransport::record` writes. All were
//! recorded with no sign-in:
//! - `claude-agent-acp-0.97.0.ndjson`: the architecture spike, which offered
//!   only v1 (the v1 path).
//! - `claude-agent-acp-0.97.0-v2-live.ndjson`: this runtime, live; the agent
//!   selects the v2 draft (token redacted).
//! - `codex-acp-0.16.0.ndjson`: v1, `session/new` needs sign-in.

mod common;

use std::{path::PathBuf, sync::Arc, time::Duration};

use agent_client_protocol::{DynConnectTo, Lines};
use chaos_core::{
    acp::{AcpAdapter, FnTransport, SpawnTransport},
    adapter::*,
    host::{AgentSpec, Core, HostConfig, ManualClock},
    types::*,
};
use common::*;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};

fn fixture(name: &str) -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// An agent that answers each request with what the recorded agent answered
/// to the same method (re-keyed to the live request id), preceded by any
/// notifications it sent first. Client frames are captured for assertions.
fn replay_transport(
    frames: Vec<Value>,
    captured: Arc<std::sync::Mutex<Vec<Value>>>,
) -> DynConnectTo<agent_client_protocol::Client> {
    let (to_agent_tx, mut to_agent_rx) = futures::channel::mpsc::unbounded::<String>();
    let (mut to_client_tx, to_client_rx) = futures::channel::mpsc::unbounded::<String>();
    tokio::spawn(async move {
        while let Some(line) = to_agent_rx.next().await {
            let msg: Value = serde_json::from_str(&line).unwrap();
            captured.lock().unwrap().push(msg.clone());
            let (Some(method), Some(id)) = (msg.get("method").and_then(Value::as_str), msg.get("id")) else {
                continue;
            };
            let Some(pos) = frames.iter().position(|f| {
                f["dir"] == "out" && f["frame"]["method"] == method
            }) else {
                let err = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not in fixture"}});
                let _ = to_client_tx.send(err.to_string()).await;
                continue;
            };
            let recorded_id = frames[pos]["frame"]["id"].clone();
            let mut answered = false;
            for f in &frames[pos + 1..] {
                if f["dir"] == "out" && answered {
                    break; // the agent was waiting for the client again
                }
                if f["dir"] != "in" {
                    continue;
                }
                let mut frame = f["frame"].clone();
                let is_answer = frame.get("id") == Some(&recorded_id) && frame.get("method").is_none();
                if answered && frame.get("method").is_none() {
                    break; // an answer to a later request
                }
                if is_answer {
                    frame["id"] = id.clone();
                    answered = true;
                }
                let _ = to_client_tx.send(frame.to_string()).await;
            }
        }
    });
    DynConnectTo::new(Lines::new(
        to_agent_tx.sink_map_err(std::io::Error::other),
        to_client_rx.map(Ok::<_, std::io::Error>),
    ))
}

async fn open_replay(name: &str) -> (Result<Arc<dyn AgentSession>, AgentError>, Vec<Value>, Option<u16>) {
    let frames = fixture(name);
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let c = captured.clone();
    let adapter = AcpAdapter::new(Arc::new(FnTransport("replay".into(), move || {
        replay_transport(frames.clone(), c.clone())
    })));
    let result = adapter
        .open(OpenSession {
            agent: MemberId(1),
            channel: ChannelId(1),
            handle: Arc::from("claude"),
            cwd: std::env::temp_dir(),
            chat_tools: ChatToolsEndpoint {
                url: "http://127.0.0.1:9/mcp".into(),
                token: "t0ken".into(),
                bridge: Some(BridgeCommand {
                    command: "/usr/local/bin/chaos-control".into(),
                    args: vec!["mcp-bridge".into()],
                }),
            },
            events: EventSink::new(|_| {}),
        })
        .await;
    let frames = captured.lock().unwrap().clone();
    (result, frames, adapter.negotiated_version())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_only_agent_replays_to_v1_with_honest_caps() {
    // The spike recorded claude-agent-acp answering a v1-only client, so here
    // it behaves as a v1-only agent: the v2 offer gets a v1 answer.
    let (session, sent, version) = open_replay("claude-agent-acp-0.97.0.ndjson").await;
    let session = session.expect("session opens");
    let info = session.info();
    assert_eq!(&*info.protocol, "acp/v1");
    assert_eq!(version, Some(1));
    assert!(info.caps.contains(Caps::CHAT_TOOLS_HTTP | Caps::CHAT_TOOLS_STDIO | Caps::INTERRUPT | Caps::RESUME));
    assert!(!info.caps.contains(Caps::STEER), "vendor steering is not assumed");
    assert!(!info.caps.contains(Caps::QUEUE), "vendor queueing is not assumed");
    let ext = info.unused_extensions.join(", ");
    assert!(ext.contains("steering") && ext.contains("promptQueueing"), "{ext}");
    assert_eq!(info.chat_tools, ChatToolsTransport::Http);
    assert_eq!(&*info.native_ref, "9eaac4b7-4e93-41f6-8ea3-a07e98948960");
    // We offered the v2 draft first, then reconnected with v1.
    let inits: Vec<&Value> = sent.iter().filter(|f| f["method"] == "initialize").collect();
    assert_eq!(inits.len(), 2);
    assert_eq!(inits[0]["params"]["protocolVersion"], json!(2));
    assert_eq!(inits[1]["params"]["protocolVersion"], json!(1));
    assert_eq!(inits[1]["params"]["clientCapabilities"]["terminal"], json!(false), "no terminal offered");
    let new = sent.iter().find(|f| f["method"] == "session/new").unwrap();
    let servers = new["params"]["mcpServers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["type"], json!("http"));
    assert_eq!(servers[0]["headers"][0]["value"], json!("Bearer t0ken"));
    assert!(sent.iter().all(|f| f["method"] != "session/set_mode"), "never touches permission modes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_agent_acp_selects_v2_and_a_signed_out_turn_goes_red() {
    let (session, sent, version) = open_replay("claude-agent-acp-0.97.0-v2-live.ndjson").await;
    let session = session.expect("session opens");
    let info = session.info();
    assert_eq!(&*info.protocol, "acp/v2-draft");
    assert_eq!(version, Some(2));
    assert!(info.caps.contains(Caps::CHAT_TOOLS_HTTP));
    assert!(!info.caps.contains(Caps::STEER) && !info.caps.contains(Caps::QUEUE));
    assert_eq!(info.chat_tools, ChatToolsTransport::Http);
    let inits: Vec<&Value> = sent.iter().filter(|f| f["method"] == "initialize").collect();
    assert_eq!(inits.len(), 1, "no fallback needed");
    session.close().await;

    // Through the runtime: accepted (prompt response), then `idle` with an
    // `error` stop "Authentication required": red, one note, no reply, no retry.
    let clock = ManualClock::new(T0);
    let (core, _events) = Core::start(HostConfig::default(), clock.clone()).await.unwrap();
    let main = core.channel("main").await.unwrap().id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let frames = fixture("claude-agent-acp-0.97.0-v2-live.ndjson");
    let adapter = Arc::new(AcpAdapter::new(Arc::new(FnTransport("replay".into(), move || {
        replay_transport(frames.clone(), Arc::default())
    }))));
    let claude = core
        .add_agent(AgentSpec { handle: "claude".into(), name: "Claude".into(), adapter, cwd: None }, main)
        .await
        .unwrap();
    let ping = core.post(hx.id, main, "@claude hello", None).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let st = core.status(claude.id, main).await.unwrap();
        if st.glow == Glow::Unresponsive {
            assert!(st.presence.unwrap().starts_with("needs sign-in"));
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never went red");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    clock.advance(60_000);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let history = core.history(main, 100).await.unwrap();
    assert!(history.iter().all(|m| m.author != Some(claude.id)), "no reply, no fallback");
    let notes: Vec<_> = history.iter().filter(|m| m.origin == Origin::System).collect();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].body.contains("needs to be signed in"), "{}", notes[0].body);
    let (state, attempts): (String, i64) = core
        .with_engine(|e, _| {
            e.conn()
                .query_row("SELECT state, attempts FROM batches", [], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!((state.as_str(), attempts), ("failed", 1), "accepted, so never re-sent");
    let _ = ping;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_acp_auth_required_turns_the_agent_red() {
    let (session, _, _) = open_replay("codex-acp-0.16.0.ndjson").await;
    let err = session.err().expect("session/new fails");
    assert_eq!(err.kind, AgentErrorKind::Auth);
    assert!(err.message.contains("Authentication required"));

    // Through the runtime: red immediately, no retry loop.
    let clock = ManualClock::new(T0);
    let (core, _events) = Core::start(HostConfig::default(), clock.clone()).await.unwrap();
    let main = core.channel("main").await.unwrap().id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let frames = fixture("codex-acp-0.16.0.ndjson");
    let adapter = Arc::new(AcpAdapter::new(Arc::new(FnTransport("replay".into(), move || {
        replay_transport(frames.clone(), Arc::default())
    }))));
    let codex = core
        .add_agent(
            AgentSpec {
                handle: "codex".into(),
                name: "Codex".into(),
                adapter,
                cwd: None,
            },
            main,
        )
        .await
        .unwrap();
    core.post(hx.id, main, "@codex hello", None).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let st = core.status(codex.id, main).await.unwrap();
        if st.glow == Glow::Unresponsive {
            assert!(st.presence.unwrap().starts_with("needs sign-in"));
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never went red");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    clock.advance(60_000);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let attempts: i64 = core
        .with_engine(|e, _| e.conn().query_row("SELECT attempts FROM batches", [], |r| r.get(0)).unwrap())
        .await
        .unwrap();
    assert_eq!(attempts, 1, "auth failures are not retried");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_agent_binary_over_real_stdio() {
    let dir = std::env::temp_dir().join(format!("cc-fake-bin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join("script.json");
    std::fs::write(
        &script_path,
        json!({"turns": [{"steps": [
            {"tool_call": "Looking"},
            {"mcp": {"tool": "chat_send", "args": {"body": "hello from a real process", "reply_to": "$TRIGGER", "client_request_id": "bin-$N"}}}
        ]}]})
        .to_string(),
    )
    .unwrap();
    let record = dir.join("frames.ndjson");
    let _ = std::fs::remove_file(&record);
    let clock = ManualClock::new(T0);
    let (core, mut events) = Core::start(HostConfig::default(), clock).await.unwrap();
    let main = core.channel("main").await.unwrap().id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(AcpAdapter::new(Arc::new(SpawnTransport {
        command: PathBuf::from(env!("CARGO_BIN_EXE_fake-acp-agent")),
        args: vec![],
        env: vec![("FAKE_ACP_SCRIPT".into(), script_path.display().to_string())],
        record: Some(record.clone()),
    })));
    let bot = core
        .add_agent(
            AgentSpec {
                handle: "bin".into(),
                name: "Binary".into(),
                adapter,
                cwd: Some(dir.clone()),
            },
            main,
        )
        .await
        .unwrap();
    let ping = core.post(hx.id, main, "@bin are you a real process?", None).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let reply = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(chaos_core::engine::CoreEvent::Message(m))) if m.author == Some(bot.id) => break m,
            Ok(Some(_)) => {}
            other => panic!("no reply: {other:?}"),
        }
    };
    assert_eq!(reply.reply_to, Some(ping.id));
    assert_eq!(&*reply.body, "hello from a real process");
    core.shutdown().await;
    let frames = std::fs::read_to_string(&record).unwrap();
    assert!(frames.lines().any(|l| l.contains("\"dir\":\"out\"") && l.contains("session/prompt")));
    assert!(frames.lines().any(|l| l.contains("\"dir\":\"in\"") && l.contains("tool_call")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Live handshake against a real agent binary. Not run in CI:
/// `CHAOS_LIVE_ACP="npx -y @agentclientprotocol/claude-agent-acp@0.97.0" cargo test -p chaos-core --test replay live -- --ignored --nocapture`
/// (or a path to an installed `claude-agent-acp`). Records the frames to
/// `$CHAOS_LIVE_RECORD` (default: a temp file) and prints the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawns a real agent; run by hand"]
async fn live_acp_agent_handshake_and_first_turn() {
    let spec = std::env::var("CHAOS_LIVE_ACP").expect("set CHAOS_LIVE_ACP");
    let mut parts = spec.split_whitespace().map(str::to_string);
    let command = PathBuf::from(parts.next().unwrap());
    let args: Vec<String> = parts.collect();
    let record = std::env::var("CHAOS_LIVE_RECORD")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("chaos-live-acp.ndjson"));
    let _ = std::fs::remove_file(&record);
    let home = std::env::var("CHAOS_LIVE_HOME").ok();
    let mut env = vec![];
    if let Some(h) = home {
        env.push(("HOME".to_string(), h));
    }
    let clock = Arc::new(chaos_core::host::SystemClock);
    let (core, mut events) = Core::start(HostConfig::default(), clock).await.unwrap();
    let main = core.channel("main").await.unwrap().id;
    let hx = core.human("hx", "Hx", main).await.unwrap();
    let adapter = Arc::new(AcpAdapter::new(Arc::new(SpawnTransport {
        command,
        args,
        env,
        record: Some(record.clone()),
    })));
    let agent = core
        .add_agent(
            AgentSpec {
                handle: "live".into(),
                name: "Live agent".into(),
                adapter: adapter.clone(),
                cwd: Some(std::env::temp_dir()),
            },
            main,
        )
        .await
        .unwrap();
    core.post(hx.id, main, "@live reply with the word pong using chat_send", None)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut last = None;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(chaos_core::engine::CoreEvent::Agent(st))) if st.agent == agent.id => {
                println!("status: {:?} {:?}", st.glow, st.presence);
                let done = st.glow == Glow::Unresponsive;
                last = Some(st);
                if done {
                    break;
                }
            }
            Ok(Some(chaos_core::engine::CoreEvent::Message(m))) => {
                println!("message ({:?}): {}", m.origin, m.body);
                if m.author == Some(agent.id) {
                    break;
                }
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    println!("negotiated ACP version: {:?}", adapter.negotiated_version());
    println!("last status: {last:?}");
    println!("frames recorded to {}", record.display());
    core.shutdown().await;
}
