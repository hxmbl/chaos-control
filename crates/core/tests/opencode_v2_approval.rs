use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    http::StatusCode,
    routing::{get, post},
};
use chaos_core::{
    adapter::{AgentAdapter, ChatToolsEndpoint, EventSink, OpenSession},
    approval::{
        ApprovalBroker, ApprovalBrokerError, ApprovalDecision, ApprovalDecisionKind, ApprovalId,
        ApprovalOutcome, ApprovalOutcomeState, ApprovalRequest,
    },
    opencode_v2::{OpenCodeV2Adapter, OpenCodeV2Config},
    types::{ChannelId, MemberId},
};
use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

struct TestApprovalBroker {
    requests: mpsc::UnboundedSender<ApprovalRequest>,
    decision: std::sync::Mutex<Option<oneshot::Receiver<ApprovalDecisionKind>>>,
    outcomes: mpsc::UnboundedSender<ApprovalOutcome>,
}

impl ApprovalBroker for TestApprovalBroker {
    fn request_decision(
        &self,
        request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalDecision, ApprovalBrokerError>> {
        let requests = self.requests.clone();
        let decision = self
            .decision
            .lock()
            .expect("decision lock")
            .take()
            .expect("one approval request");
        Box::pin(async move {
            requests
                .send(request.clone())
                .map_err(|_| ApprovalBrokerError::new("request receiver closed"))?;
            let decision = decision
                .await
                .map_err(|_| ApprovalBrokerError::new("decision sender closed"))?;
            Ok(ApprovalDecision {
                cc_request_id: request.cc_request_id,
                decision,
                decided_at: 2,
                actor: Some(Arc::from("tester")),
            })
        })
    }

    fn record_outcome(
        &self,
        outcome: ApprovalOutcome,
    ) -> BoxFuture<'_, Result<(), ApprovalBrokerError>> {
        let outcomes = self.outcomes.clone();
        Box::pin(async move {
            outcomes
                .send(outcome)
                .map_err(|_| ApprovalBrokerError::new("outcome receiver closed"))
        })
    }
}

#[tokio::test]
async fn v2_permission_uses_correlated_arguments_and_fails_closed_when_unresolved() {
    for (choice, expected_state) in [
        (Some("once"), ApprovalOutcomeState::Allowed),
        (Some("reject"), ApprovalOutcomeState::Denied),
        (None, ApprovalOutcomeState::NotSent),
    ] {
        let (request_tx, mut request_rx) = mpsc::unbounded_channel();
        let (choice_tx, choice_rx) = oneshot::channel();
        let (outcome_tx, mut outcome_rx) = mpsc::unbounded_channel();
        let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
        let app = Router::new()
            .route(
                "/api/session",
                post(|| async { Json(serde_json::json!({"data":{"id":"s1"}})) }),
            )
            .route(
                "/api/event",
                get(|| async {
                    let body = concat!(
                        "data: {\"type\":\"session.tool.input.ended\",\"data\":",
                        "{\"sessionID\":\"s1\",\"id\":\"tool-1\",\"input\":{\"command\":\"echo SAFE\"}}}\n\n",
                        "data: {\"type\":\"permission.asked\",\"data\":",
                        "{\"id\":\"p1\",\"sessionID\":\"s1\",\"action\":\"shell\",",
                        "\"resources\":[\"echo SAFE\"],\"save\":[\"echo *\"],",
                        "\"source\":{\"type\":\"tool\",\"messageID\":\"m1\",\"id\":\"tool-1\"}}}\n\n"
                    );
                    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body)
                }),
            )
            .route(
                "/api/session/s1/permission/p1/reply",
                post({
                    let reply_tx = reply_tx.clone();
                    move |Json(body): Json<Value>| {
                        let reply_tx = reply_tx.clone();
                        async move {
                            let _ = reply_tx.send(body);
                            StatusCode::NO_CONTENT
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let broker = Arc::new(TestApprovalBroker {
            requests: request_tx,
            decision: std::sync::Mutex::new(Some(choice_rx)),
            outcomes: outcome_tx,
        });
        let adapter = OpenCodeV2Adapter::new(OpenCodeV2Config {
            command: "opencode".into(),
            args: Vec::new(),
            env: Vec::new(),
            server_url: Some(format!("http://{addr}")),
            server_password: None,
            model: None,
        });
        let session = adapter
            .open(OpenSession {
                agent: MemberId(7),
                channel: ChannelId(4),
                handle: "tester".into(),
                cwd: std::env::temp_dir(),
                chat_tools: ChatToolsEndpoint {
                    url: String::new(),
                    token: String::new(),
                    bridge: None,
                },
                events: EventSink::new(|_| {}),
                approval_broker: Some(broker),
            })
            .await
            .expect("open session");

        let request = tokio::time::timeout(Duration::from_secs(2), request_rx.recv())
            .await
            .expect("approval request arrives")
            .expect("request channel open");
        assert_eq!(request.cc_request_id, ApprovalId::from("opencode-v2:s1:p1"));
        assert_eq!(request.agent, MemberId(7));
        assert_eq!(request.channel, ChannelId(4));
        assert_eq!(request.operation.action.as_ref(), "shell");
        assert_eq!(request.operation.summary.as_deref(), Some("echo SAFE"));
        assert_eq!(
            request.operation.arguments,
            Some(serde_json::json!({"command":"echo SAFE"}))
        );
        assert_eq!(request.context.as_ref().unwrap()["id"], "tool-1");

        let decision = match choice {
            Some(choice) => ApprovalDecisionKind::Selected {
                choice: Arc::from(choice),
            },
            None => ApprovalDecisionKind::NotResolved {
                resolution: chaos_core::approval::ApprovalResolution::Cancelled,
            },
        };
        choice_tx.send(decision).unwrap();
        if let Some(choice) = choice {
            let reply = tokio::time::timeout(Duration::from_secs(2), reply_rx.recv())
                .await
                .expect("provider reply arrives")
                .expect("reply channel open");
            assert_eq!(reply["decision"], choice);
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(50), reply_rx.recv())
                    .await
                    .is_err(),
                "unresolved approval must not reply to the provider"
            );
        }
        let outcome = tokio::time::timeout(Duration::from_secs(2), outcome_rx.recv())
            .await
            .expect("outcome arrives")
            .expect("outcome channel open");
        assert_eq!(outcome.cc_request_id, request.cc_request_id);
        assert_eq!(outcome.state, expected_state);
        session.close().await;
    }
}
