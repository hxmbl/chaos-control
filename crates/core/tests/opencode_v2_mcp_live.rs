//! Ignored live probe: does an injected MCP tool actually reach the model?
//!
//! Run explicitly with:
//!   cargo test -p chaos-core --test opencode_v2_mcp_live -- --ignored --nocapture

use chaos_core::adapter::{
    AgentAdapter, AgentEvent, ChatToolsEndpoint, EventSink, OpenSession, TurnOutcome,
};
use chaos_core::opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config};
use chaos_core::types::{BatchId, ChannelId, MemberId};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig,
    },
    schemars,
    service::RequestContext,
    tool, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoRequest {
    /// Text to echo back.
    text: String,
}

#[derive(Clone)]
struct EchoServer {
    tool_router: ToolRouter<Self>,
    listed: Arc<tokio::sync::Notify>,
    called: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
}

#[tool_router]
impl EchoServer {
    fn new(
        listed: Arc<tokio::sync::Notify>,
        called: Arc<tokio::sync::Notify>,
        calls: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            listed,
            called,
            calls,
        }
    }

    #[tool(description = "Echo the given text back with an ECHO: prefix")]
    fn echo(&self, Parameters(EchoRequest { text }): Parameters<EchoRequest>) -> String {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.called.notify_one();
        format!("ECHO:{text}")
    }
}

impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Tiny echo server for the chaos-control MCP probe")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.listed.notify_one();
        Ok(ListToolsResult::with_all_items(self.tool_router.list_all()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let text = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("text"))
            .and_then(|value| value.as_str())
            .unwrap_or("");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.called.notify_one();
        Ok(CallToolResult::success(vec![ContentBlock::text(format!("ECHO:{text}"))]).into())
    }
}

#[tokio::test]
#[ignore = "live: spawns opencode serve and uses a real model"]
async fn injected_mcp_tool_reaches_the_model() {
    let listed = Arc::new(tokio::sync::Notify::new());
    let called = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));

    let service: StreamableHttpService<EchoServer, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let (listed, called, calls) = (listed.clone(), called.clone(), calls.clone());
                move || {
                    Ok(EchoServer::new(
                        listed.clone(),
                        called.clone(),
                        calls.clone(),
                    ))
                }
            },
            Default::default(),
            StreamableHttpServerConfig::default()
                .with_json_response(true)
                .with_sse_keep_alive(None),
        );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake MCP");
    let mcp_url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let mcp_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let adapter = OpenCodeV2Adapter::new(OpenCodeV2Config {
        command: "opencode".into(),
        args: Vec::new(),
        env: Vec::new(),
        server_url: None,
        server_password: None,
        model: Some(Arc::from("opencode/big-pickle")),
    });
    let session = adapter
        .open(OpenSession {
            agent: MemberId(0),
            channel: ChannelId(0),
            handle: "mcp-probe".into(),
            cwd: std::env::temp_dir(),
            chat_tools: ChatToolsEndpoint {
                url: mcp_url,
                token: String::new(),
                bridge: None,
            },
            events: EventSink::new(move |event| {
                let _ = tx.send(event);
            }),
            approval_broker: None,
        })
        .await
        .expect("open session");
    assert_eq!(
        session.info().chat_tools,
        chaos_core::adapter::ChatToolsTransport::Http
    );

    tokio::time::timeout(Duration::from_secs(30), listed.notified())
        .await
        .expect("opencode discovered the injected MCP tools (tools/list)");

    session
        .deliver(
            BatchId(1),
            Arc::from("Call echo with text MAGIC and reply with exactly its result"),
        )
        .await
        .expect("deliver");

    let mut final_text = None;
    let mut failure = None;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(120), rx.recv()).await {
        eprintln!("event: {event:?}");
        match event {
            AgentEvent::TurnEnded {
                turn: BatchId(1),
                outcome: TurnOutcome::Completed,
                final_text: text,
            } => {
                final_text = Some(text);
                break;
            }
            AgentEvent::TurnFailed {
                turn: BatchId(1),
                error,
                ..
            } => {
                failure = Some(error.to_string());
                break;
            }
            _ => {}
        }
    }

    if let Some(error) = failure {
        panic!("turn failed: {error}");
    }
    let text = final_text.expect("turn ended");
    assert!(
        text.contains("ECHO:MAGIC"),
        "model did not use the injected tool; final_text = {text:?}"
    );
    assert!(calls.load(Ordering::SeqCst) > 0, "echo tool was not called");

    session.close().await;
    mcp_task.abort();
}
