//! The in-process runtime: one actor task owns the [`Engine`], the agent
//! sessions and the chat MCP server, behind the cloneable [`Core`] handle.
//!
//! The handle is the API boundary that could later move behind a daemon: every
//! call is a message to the actor, every answer comes back on a channel, and UI
//! updates arrive as [`CoreEvent`]s.

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    adapter::*,
    approval::{
        ApprovalBroker, ApprovalBrokerError, ApprovalDecision, ApprovalDecisionKind, ApprovalId,
        ApprovalRequest, ApprovalResolution, ApprovalScope,
    },
    engine::{
        AgentStatus, CoreEvent, Dispatch, Effect, Engine, PostError, PostRequest, Reacted,
        StanceRequest,
    },
    lifecycle::{
        ContextRefresh, ControlOutcome, DeliveryDecision, InterruptOutcome, LifecycleScope,
        LifecycleState, MembershipState, RefreshOutcome, SleepApplied,
    },
    mcp::{self, Scope, Tokens},
    policy::Policy,
    reaction::PingAnswers,
    store::{OpenError, Store},
    types::*,
};

// ---------- clock ----------

/// The runtime's only source of time. Tests use [`ManualClock`].
pub trait Clock: Send + Sync {
    fn now(&self) -> Millis;
    fn sleep_until(&self, deadline: Millis) -> BoxFuture<'static, ()>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Millis {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as Millis)
            .unwrap_or_default()
    }

    fn sleep_until(&self, deadline: Millis) -> BoxFuture<'static, ()> {
        let wait = deadline.saturating_sub(self.now());
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(wait)))
    }
}

/// A clock that only moves when told to.
pub struct ManualClock {
    now: watch::Sender<Millis>,
}

impl ManualClock {
    pub fn new(start: Millis) -> Arc<Self> {
        Arc::new(ManualClock {
            now: watch::channel(start).0,
        })
    }

    pub fn advance(&self, ms: Millis) {
        self.now.send_modify(|t| *t += ms);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Millis {
        *self.now.borrow()
    }

    fn sleep_until(&self, deadline: Millis) -> BoxFuture<'static, ()> {
        let mut rx = self.now.subscribe();
        Box::pin(async move {
            let _ = rx.wait_for(|t| *t >= deadline).await;
        })
    }
}

// ---------- config ----------

pub struct HostConfig {
    /// SQLite file; `None` for an in-memory database.
    pub db: Option<PathBuf>,
    pub policy: Policy,
    /// Where the chat MCP server listens (port 0 = ephemeral).
    pub mcp_bind: SocketAddr,
    /// How agents can start the stdio bridge, if this process can be one.
    pub bridge: Option<BridgeCommand>,
    /// Working directory for agents that don't set their own.
    pub default_cwd: PathBuf,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig {
            db: None,
            policy: Policy::default(),
            mcp_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            bridge: None,
            default_cwd: std::env::temp_dir(),
        }
    }
}

pub struct AgentSpec {
    pub handle: String,
    pub name: String,
    pub adapter: Arc<dyn AgentAdapter>,
    pub cwd: Option<PathBuf>,
    /// The agent's own list of reactions that answer a ping
    /// (`ping_answer_reactions` in `agents.json`); `None` uses the project's
    /// setting, then the default (see `Engine::ping_answers`).
    pub ping_answers: Option<PingAnswers>,
}

// ---------- handle ----------

type EngineFn = Box<dyn FnOnce(&mut Engine, Millis) + Send>;

enum Cmd {
    Engine(EngineFn),
    AddAgent {
        spec: AgentSpec,
        channel: ChannelId,
        reply: oneshot::Sender<Result<Member, String>>,
    },
    Opened {
        agent: MemberId,
        channel: ChannelId,
        epoch: u64,
        result: Result<Arc<dyn AgentSession>, AgentError>,
    },
    Event {
        agent: MemberId,
        channel: ChannelId,
        epoch: u64,
        event: AgentEvent,
    },
    DeliverFailed {
        agent: MemberId,
        channel: ChannelId,
        epoch: u64,
        batch: BatchId,
        error: AgentError,
    },
    Interrupt {
        agent: MemberId,
        channel: ChannelId,
    },
    /// Scoped interruption with an observable per-session result.
    InterruptScoped {
        agent: MemberId,
        channels: Vec<ChannelId>,
        fallback_close: bool,
        reply: oneshot::Sender<Vec<(ChannelId, InterruptOutcome)>>,
    },
    /// A session was closed as the fallback for an interrupt the integration
    /// could not confirm. Drop the owned slot and settle the killed turn so a
    /// later message can reconnect instead of waiting on a dead session.
    SlotClosed {
        agent: MemberId,
        channel: ChannelId,
        epoch: u64,
    },
    /// A high-priority runtime instruction, sent straight to a live session.
    /// It deliberately bypasses the message graph: no batch, no obligation, no
    /// chat message, and no claim that the agent complied.
    DeliverInstruction {
        agent: MemberId,
        channel: ChannelId,
        prompt: Arc<str>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RemoveMember {
        project: ProjectId,
        member: MemberId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Shutdown(oneshot::Sender<()>),
}

/// A model member to add (or re-register at startup): which model, through
/// which harness, and the adapter that harness gets for it.
pub struct MemberSpec {
    pub harness: String,
    /// The harness's model id; `None` runs its default model.
    pub model: Option<String>,
    pub model_name: String,
    pub adapter: Arc<dyn AgentAdapter>,
    pub cwd: Option<PathBuf>,
}

/// Cheap, cloneable handle to the runtime.
#[derive(Clone)]
pub struct Core {
    tx: mpsc::UnboundedSender<Cmd>,
    mcp_url: Arc<str>,
    tokens: Arc<Tokens>,
    approvals: Arc<RuntimeApprovalBroker>,
    /// The tokio runtime core was started on. Anything that touches a
    /// harness (process spawns, pipes, timers, `tokio::spawn`) runs here,
    /// never in the caller's future: the app awaits core from GPUI's
    /// executor, which is not a tokio runtime, and tokio I/O polled there
    /// panics ("there is no reactor running").
    rt: tokio::runtime::Handle,
}

const GONE: &str = "chaos-control core has stopped";

/// Why [`Core::start`] failed.
#[derive(Debug)]
pub enum StartError {
    /// The database was written by a newer build. It was left untouched, and
    /// the app should say so rather than run without it.
    NewerData(OpenError),
    Other(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::NewerData(e) => write!(f, "{e}"),
            StartError::Other(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for StartError {}

impl From<String> for StartError {
    fn from(e: String) -> Self {
        StartError::Other(e)
    }
}

struct PendingApproval {
    request: ApprovalRequest,
    decision: oneshot::Sender<ApprovalDecision>,
}

struct RuntimeApprovalBroker {
    events: mpsc::UnboundedSender<CoreEvent>,
    engine_tx: Option<mpsc::UnboundedSender<Cmd>>,
    clock: Arc<dyn Clock>,
    pending: std::sync::Mutex<HashMap<ApprovalId, PendingApproval>>,
    outcomes: std::sync::Mutex<HashMap<ApprovalId, crate::approval::ApprovalOutcome>>,
    /// Ids whose outcome write failed. They still block replay in this
    /// process, but are not published as recorded outcomes: a restart cannot
    /// recover them, so nothing should claim they were stored.
    unpersisted: std::sync::Mutex<HashSet<ApprovalId>>,
}

impl RuntimeApprovalBroker {
    fn new(
        events: mpsc::UnboundedSender<CoreEvent>,
        engine_tx: Option<mpsc::UnboundedSender<Cmd>>,
        clock: Arc<dyn Clock>,
        outcomes: HashMap<ApprovalId, crate::approval::ApprovalOutcome>,
    ) -> Self {
        Self {
            events,
            engine_tx,
            clock,
            pending: std::sync::Mutex::new(HashMap::new()),
            outcomes: std::sync::Mutex::new(outcomes),
            unpersisted: std::sync::Mutex::new(HashSet::new()),
        }
    }

    /// Fail closed the pending requests accepted by `matches`. Called from
    /// session-level cleanup, so a failing session cannot answer requests that
    /// belong to another agent.
    fn cancel_matching(
        &self,
        matches: impl Fn(&ApprovalRequest) -> bool,
        resolution: ApprovalResolution,
    ) -> Result<(), ApprovalBrokerError> {
        let mut pending_map = self
            .pending
            .lock()
            .map_err(|_| ApprovalBrokerError::new("approval broker lock poisoned"))?;
        let ids: Vec<ApprovalId> = pending_map
            .iter()
            .filter(|(_, entry)| matches(&entry.request))
            .map(|(id, _)| id.clone())
            .collect();
        let cancelled: Vec<_> = ids
            .into_iter()
            .filter_map(|id| pending_map.remove(&id).map(|entry| (id, entry.decision)))
            .collect();
        drop(pending_map);
        for (id, decision) in cancelled {
            decision
                .send(ApprovalDecision {
                    cc_request_id: id,
                    decision: ApprovalDecisionKind::NotResolved { resolution },
                    decided_at: self.clock.now(),
                    actor: None,
                })
                .map_err(|_| ApprovalBrokerError::new("approval request was already abandoned"))?;
        }
        Ok(())
    }

    fn resolve(&self, id: &ApprovalId, choice: &str) -> Result<(), String> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| "approval broker lock poisoned".to_string())?;
        let request = pending
            .get(id)
            .ok_or_else(|| format!("approval request {} is no longer pending", id.as_str()))?;
        let choices = match &request.request.choices {
            crate::approval::ApprovalChoices::DecisionVocabulary { decisions, .. } => decisions
                .iter()
                .any(|candidate| candidate.as_ref() == choice),
            crate::approval::ApprovalChoices::ProviderOptions { options } => {
                options.iter().any(|option| option.id.as_ref() == choice)
            }
        };
        if !choices {
            return Err(format!("choice {choice:?} is not valid for this approval"));
        }
        let entry = pending
            .remove(id)
            .ok_or_else(|| format!("approval request {} is no longer pending", id.as_str()))?;
        drop(pending);
        entry
            .decision
            .send(ApprovalDecision {
                cc_request_id: id.clone(),
                decision: ApprovalDecisionKind::Selected {
                    choice: Arc::from(choice),
                },
                decided_at: self.clock.now(),
                actor: None,
            })
            .map_err(|_| format!("approval request {} was already abandoned", id.as_str()))
    }

    fn cancel(&self, id: &ApprovalId) -> Result<(), String> {
        let pending = self
            .pending
            .lock()
            .map_err(|_| "approval broker lock poisoned".to_string())?
            .remove(id)
            .ok_or_else(|| format!("approval request {} is no longer pending", id.as_str()))?;
        pending
            .decision
            .send(ApprovalDecision {
                cc_request_id: id.clone(),
                decision: ApprovalDecisionKind::NotResolved {
                    resolution: ApprovalResolution::Cancelled,
                },
                decided_at: self.clock.now(),
                actor: None,
            })
            .map_err(|_| format!("approval request {} was already abandoned", id.as_str()))
    }
}

impl ApprovalBroker for RuntimeApprovalBroker {
    fn request_decision(
        &self,
        request: ApprovalRequest,
    ) -> futures::future::BoxFuture<'_, Result<ApprovalDecision, ApprovalBrokerError>> {
        Box::pin(async move {
            let id = request.cc_request_id.clone();
            let (decision, wait) = oneshot::channel();
            {
                let mut pending = self
                    .pending
                    .lock()
                    .map_err(|_| ApprovalBrokerError::new("approval broker lock poisoned"))?;
                if pending.contains_key(&id) {
                    return Err(ApprovalBrokerError::new(format!(
                        "approval request {} is already pending",
                        id.as_str()
                    )));
                }
                // Replay guard: an id that already resolved — user decision,
                // fail-closed cleanup, an outcome recovered from the store, or
                // one whose store write failed — must never reopen as a new
                // request.
                let recorded = self
                    .outcomes
                    .lock()
                    .map_err(|_| ApprovalBrokerError::new("approval broker lock poisoned"))?
                    .contains_key(&id);
                let unpersisted = self
                    .unpersisted
                    .lock()
                    .map_err(|_| ApprovalBrokerError::new("approval broker lock poisoned"))?
                    .contains(&id);
                if recorded || unpersisted {
                    return Err(ApprovalBrokerError::new(format!(
                        "approval request {} is already resolved",
                        id.as_str()
                    )));
                }
                pending.insert(
                    id.clone(),
                    PendingApproval {
                        request: request.clone(),
                        decision,
                    },
                );
            }
            if self
                .events
                .send(CoreEvent::ApprovalRequested(Box::new(request)))
                .is_err()
            {
                self.pending
                    .lock()
                    .map_err(|_| ApprovalBrokerError::new("approval broker lock poisoned"))?
                    .remove(&id);
                return Err(ApprovalBrokerError::new("approval event receiver is gone"));
            }
            let result = wait.await.map_err(|_| {
                ApprovalBrokerError::new("approval request was abandoned before resolution")
            });
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&id);
            }
            result
        })
    }

    fn record_outcome(
        &self,
        outcome: crate::approval::ApprovalOutcome,
    ) -> futures::future::BoxFuture<'_, Result<(), ApprovalBrokerError>> {
        Box::pin(async move {
            // Persist before publishing. Otherwise a failed write leaves this
            // process believing in an outcome that a restart forgets, and the
            // UI showing a decision that was never recorded.
            let persisted = if let Some(tx) = &self.engine_tx {
                let (reply, wait) = oneshot::channel();
                let outcome_for_store = outcome.clone();
                let f: EngineFn = Box::new(move |engine, _now| {
                    let result = engine
                        .record_approval_outcome(&outcome_for_store)
                        .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                });
                if tx.send(Cmd::Engine(f)).is_err() {
                    Err("core runtime is gone".to_string())
                } else {
                    match wait.await {
                        Ok(result) => result,
                        Err(_) => Err("approval outcome store was abandoned".to_string()),
                    }
                }
            } else {
                Ok(())
            };

            if let Err(error) = persisted {
                // The decision may already have reached the provider, so keep
                // the id blocked for replay in this process. It is deliberately
                // not added to `outcomes`: that map is the durable record.
                self.unpersisted
                    .lock()
                    .map_err(|_| ApprovalBrokerError::new("approval outcome lock poisoned"))?
                    .insert(outcome.cc_request_id.clone());
                return Err(ApprovalBrokerError::new(error));
            }

            self.unpersisted
                .lock()
                .map_err(|_| ApprovalBrokerError::new("approval outcome lock poisoned"))?
                .remove(&outcome.cc_request_id);
            self.outcomes
                .lock()
                .map_err(|_| ApprovalBrokerError::new("approval outcome lock poisoned"))?
                .insert(outcome.cc_request_id.clone(), outcome.clone());
            if self
                .events
                .send(CoreEvent::ApprovalOutcome(outcome.clone()))
                .is_err()
            {
                return Err(ApprovalBrokerError::new("approval event receiver is gone"));
            }
            Ok(())
        })
    }

    fn cancel_pending(
        &self,
        resolution: ApprovalResolution,
    ) -> futures::future::BoxFuture<'_, Result<(), ApprovalBrokerError>> {
        Box::pin(async move { self.cancel_matching(|_| true, resolution) })
    }

    fn cancel_pending_for(
        &self,
        scope: ApprovalScope,
        resolution: ApprovalResolution,
    ) -> futures::future::BoxFuture<'_, Result<(), ApprovalBrokerError>> {
        Box::pin(async move {
            self.cancel_matching(
                |request| {
                    request.provider == scope.provider && request.session_id == scope.session_id
                },
                resolution,
            )
        })
    }
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod approval_tests {
    use super::*;
    use crate::approval::{
        ApprovalChoices, ApprovalOperation, ApprovalOutcome, ApprovalOutcomeState,
        ApprovalProvider, ApprovalScope,
    };

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            cc_request_id: ApprovalId::from("opencode-v2:s1:p1"),
            provider: ApprovalProvider::OpenCodeV2,
            provider_request_ref: Arc::from("p1"),
            session_id: Arc::from("s1"),
            agent: MemberId(7),
            channel: ChannelId(4),
            turn: Some(BatchId(9)),
            operation: ApprovalOperation {
                action: Arc::from("shell"),
                tool_name: None,
                summary: Some(Arc::from("echo OK")),
                arguments: None,
                raw: None,
            },
            choices: ApprovalChoices::DecisionVocabulary {
                decisions: vec![Arc::from("reject"), Arc::from("once")],
                raw: None,
            },
            requested_at: 1,
            context: None,
        }
    }

    #[tokio::test]
    async fn runtime_approval_broker_emits_validates_and_records_decisions() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let broker = Arc::new(RuntimeApprovalBroker::new(
            events_tx,
            None,
            Arc::new(SystemClock),
            HashMap::new(),
        ));
        let pending_id = request().cc_request_id;
        let pending_broker = broker.clone();
        let pending = tokio::spawn(async move { pending_broker.request_decision(request()).await });

        assert!(matches!(
            events_rx.recv().await,
            Some(CoreEvent::ApprovalRequested(_))
        ));
        assert!(broker.resolve(&pending_id, "always").is_err());
        broker.resolve(&pending_id, "once").unwrap();
        let decision = pending.await.unwrap().unwrap();
        assert_eq!(decision.cc_request_id, pending_id);
        assert_eq!(
            decision.decision,
            ApprovalDecisionKind::Selected {
                choice: Arc::from("once")
            }
        );

        let outcome = ApprovalOutcome {
            cc_request_id: pending_id.clone(),
            state: ApprovalOutcomeState::Allowed,
            provider_reply: None,
            execution_effect: None,
            error: None,
            recorded_at: Some(2),
        };
        broker.record_outcome(outcome.clone()).await.unwrap();
        assert_eq!(
            events_rx.recv().await,
            Some(CoreEvent::ApprovalOutcome(outcome.clone()))
        );
        assert_eq!(
            broker.outcomes.lock().unwrap().get(&pending_id),
            Some(&outcome)
        );
    }

    #[tokio::test]
    async fn approval_decisions_use_the_injected_clock() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let clock = ManualClock::new(1234);
        let broker = Arc::new(RuntimeApprovalBroker::new(
            events_tx,
            None,
            clock.clone(),
            HashMap::new(),
        ));
        let pending_id = request().cc_request_id;
        let pending_broker = broker.clone();
        let pending = tokio::spawn(async move { pending_broker.request_decision(request()).await });
        assert!(matches!(
            events_rx.recv().await,
            Some(CoreEvent::ApprovalRequested(_))
        ));

        clock.advance(50);
        broker.cancel(&pending_id).unwrap();

        let decision = pending.await.unwrap().unwrap();
        assert_eq!(decision.decided_at, 1284);
    }

    #[tokio::test]
    async fn runtime_approval_broker_rejects_duplicate_requests_and_cancels_once() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let broker = Arc::new(RuntimeApprovalBroker::new(
            events_tx,
            None,
            Arc::new(SystemClock),
            HashMap::new(),
        ));
        let pending_id = request().cc_request_id;

        let first_broker = broker.clone();
        let first = tokio::spawn(async move { first_broker.request_decision(request()).await });
        assert!(matches!(
            events_rx.recv().await,
            Some(CoreEvent::ApprovalRequested(_))
        ));

        let duplicate = broker.request_decision(request()).await.unwrap_err();
        assert!(duplicate.message.as_ref().contains("already pending"));

        broker.cancel(&pending_id).unwrap();
        let decision = first.await.unwrap().unwrap();
        assert_eq!(
            decision.decision,
            ApprovalDecisionKind::NotResolved {
                resolution: ApprovalResolution::Cancelled,
            }
        );
        assert!(broker.cancel(&pending_id).is_err());
    }

    #[tokio::test]
    async fn runtime_approval_broker_lost_event_callback_is_fail_closed() {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        drop(events_rx);
        let broker = Arc::new(RuntimeApprovalBroker::new(
            events_tx,
            None,
            Arc::new(SystemClock),
            HashMap::new(),
        ));
        let pending_id = request().cc_request_id;

        let result = broker.request_decision(request()).await;

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .as_ref()
                .contains("event receiver is gone")
        );
        assert!(broker.pending.lock().unwrap().get(&pending_id).is_none());
        assert!(broker.resolve(&pending_id, "once").is_err());
        assert!(broker.cancel(&pending_id).is_err());
    }

    #[tokio::test]
    async fn runtime_approval_broker_cancel_pending_resolves_as_disconnect() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let broker = Arc::new(RuntimeApprovalBroker::new(
            events_tx,
            None,
            Arc::new(SystemClock),
            HashMap::new(),
        ));
        let pending_id = request().cc_request_id;
        let pending_broker = broker.clone();
        let pending = tokio::spawn(async move { pending_broker.request_decision(request()).await });

        assert!(matches!(
            events_rx.recv().await,
            Some(CoreEvent::ApprovalRequested(_))
        ));
        broker
            .cancel_pending(ApprovalResolution::Disconnect)
            .await
            .unwrap();

        let decision = pending.await.unwrap().unwrap();
        assert_eq!(
            decision.decision,
            ApprovalDecisionKind::NotResolved {
                resolution: ApprovalResolution::Disconnect,
            }
        );
        assert!(broker.pending.lock().unwrap().is_empty());
        assert!(broker.resolve(&pending_id, "once").is_err());
    }

    #[tokio::test]
    async fn runtime_approval_broker_rejects_replays_after_resolution() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let broker = Arc::new(RuntimeApprovalBroker::new(
            events_tx,
            None,
            Arc::new(SystemClock),
            HashMap::new(),
        ));
        let pending_id = request().cc_request_id;

        let first_broker = broker.clone();
        let first = tokio::spawn(async move { first_broker.request_decision(request()).await });
        assert!(matches!(
            events_rx.recv().await,
            Some(CoreEvent::ApprovalRequested(_))
        ));
        broker.resolve(&pending_id, "once").unwrap();
        let decision = first.await.unwrap().unwrap();
        assert_eq!(
            decision.decision,
            ApprovalDecisionKind::Selected {
                choice: Arc::from("once")
            }
        );
        broker
            .record_outcome(ApprovalOutcome {
                cc_request_id: pending_id.clone(),
                state: ApprovalOutcomeState::Allowed,
                provider_reply: None,
                execution_effect: None,
                error: None,
                recorded_at: Some(2),
            })
            .await
            .unwrap();

        // A replay of the resolved id must fail closed; it must not reopen
        // the approval as a new request.
        let replay = broker.request_decision(request()).await.unwrap_err();
        assert!(
            replay.message.as_ref().contains("already resolved"),
            "unexpected replay error: {}",
            replay.message
        );
        assert!(broker.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelling_one_session_leaves_other_sessions_pending() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let clock = ManualClock::new(1000);
        let broker = Arc::new(RuntimeApprovalBroker::new(
            events_tx,
            None,
            clock.clone(),
            HashMap::new(),
        ));

        let mut first_request = request();
        first_request.cc_request_id = ApprovalId::from("opencode-v2:s1:p1");
        first_request.session_id = Arc::from("s1");
        let second_id = ApprovalId::from("opencode-v2:s2:p2");
        let mut second_request = request();
        second_request.cc_request_id = second_id.clone();
        second_request.session_id = Arc::from("s2");
        second_request.agent = MemberId(8);

        let first_broker = broker.clone();
        let first = tokio::spawn(async move { first_broker.request_decision(first_request).await });
        let second_broker = broker.clone();
        let second =
            tokio::spawn(async move { second_broker.request_decision(second_request).await });
        assert!(matches!(
            events_rx.recv().await,
            Some(CoreEvent::ApprovalRequested(_))
        ));
        assert!(matches!(
            events_rx.recv().await,
            Some(CoreEvent::ApprovalRequested(_))
        ));

        clock.advance(25);
        broker
            .cancel_pending_for(
                ApprovalScope::new(ApprovalProvider::OpenCodeV2, Arc::from("s1")),
                ApprovalResolution::Disconnect,
            )
            .await
            .unwrap();

        let first_decision = first.await.unwrap().unwrap();
        assert_eq!(
            first_decision.decision,
            ApprovalDecisionKind::NotResolved {
                resolution: ApprovalResolution::Disconnect,
            }
        );
        assert_eq!(first_decision.decided_at, 1025);

        // The other session's request is still pending and answerable.
        assert!(broker.pending.lock().unwrap().contains_key(&second_id));
        broker.resolve(&second_id, "once").unwrap();
        let second_decision = second.await.unwrap().unwrap();
        assert_eq!(
            second_decision.decision,
            ApprovalDecisionKind::Selected {
                choice: Arc::from("once")
            }
        );
    }

    #[tokio::test]
    async fn runtime_approval_broker_does_not_publish_an_unpersisted_outcome() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        // Remove the outcome table so the engine write fails while the broker
        // is otherwise live.
        let store = Store::open_in_memory().unwrap();
        store
            .conn()
            .execute_batch("DROP TABLE approval_outcomes")
            .unwrap();
        let mut engine = Engine::new(store, Policy::default(), 1).unwrap();
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(async move {
            while let Some(cmd) = engine_rx.recv().await {
                if let Cmd::Engine(f) = cmd {
                    f(&mut engine, 2);
                }
            }
        });

        let broker = Arc::new(RuntimeApprovalBroker::new(
            events_tx,
            Some(engine_tx),
            Arc::new(SystemClock),
            HashMap::new(),
        ));
        let outcome = ApprovalOutcome {
            cc_request_id: ApprovalId::from("opencode-v2:s1:p1"),
            state: ApprovalOutcomeState::Allowed,
            provider_reply: None,
            execution_effect: None,
            error: None,
            recorded_at: Some(2),
        };
        assert!(broker.record_outcome(outcome).await.is_err());

        // A store failure must not be published as a recorded outcome: the UI
        // would show a decision that a restart cannot recover.
        assert!(events_rx.try_recv().is_err());

        // Fail closed: the decision may already have reached the provider, so
        // a replay must not re-open the request in this process.
        let replay = broker.request_decision(request()).await.unwrap_err();
        assert!(
            replay.message.as_ref().contains("already resolved"),
            "unexpected replay error: {}",
            replay.message
        );

        drop(broker);
        driver.await.unwrap();
    }
}

impl Core {
    /// Starts the runtime on the current tokio runtime. Returns the handle and
    /// the stream of UI events.
    pub async fn start(
        config: HostConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<(Core, mpsc::UnboundedReceiver<CoreEvent>), StartError> {
        let store = match &config.db {
            // `Store::open` creates the directory privately when needed.
            Some(path) => Store::open(path),
            None => Store::open_in_memory(),
        }
        .map_err(|e| match e {
            OpenError::NewerSchema { .. } => StartError::NewerData(e),
            e => StartError::Other(format!("open database: {e}")),
        })?;
        let engine = Engine::new(store, config.policy.clone(), clock.now())
            .map_err(|e| format!("recover: {e}"))?;
        let (tx, rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let approval_outcomes = engine
            .approval_outcomes()
            .map_err(|e| format!("recover approval outcomes: {e}"))?;
        let approvals = Arc::new(RuntimeApprovalBroker::new(
            events_tx.clone(),
            Some(tx.clone()),
            clock.clone(),
            approval_outcomes,
        ));
        let tokens = Arc::new(Tokens::default());
        let caller_tx = tx.clone();
        let caller: mcp::ToolCaller = Arc::new(move |scope: Scope, name: String, args: Value| {
            let tx = caller_tx.clone();
            Box::pin(async move {
                let (reply, wait) = oneshot::channel();
                let f: EngineFn = Box::new(move |engine, now| {
                    let r = mcp::call_tool(engine, now, scope, &name, &args).map_err(|e| e.0);
                    let _ = reply.send(r);
                });
                tx.send(Cmd::Engine(f)).map_err(|_| GONE.to_string())?;
                wait.await.map_err(|_| GONE.to_string())?
            })
        });
        let mcp_url = mcp::serve(config.mcp_bind, tokens.clone(), caller)
            .await
            .map_err(|e| format!("start chat MCP server: {e}"))?;
        let actor = Actor {
            engine,
            clock,
            tokens: tokens.clone(),
            mcp_url: mcp_url.clone(),
            bridge: config.bridge,
            default_cwd: config.default_cwd,
            agents: HashMap::new(),
            slots: HashMap::new(),
            approvals: approvals.clone(),
            next_epoch: 1,
            tx: tx.clone(),
            events: events_tx,
            alive: true,
        };
        tokio::spawn(actor.run(rx));
        Ok((
            Core {
                tx,
                mcp_url: Arc::from(mcp_url),
                tokens,
                approvals,
                rt: tokio::runtime::Handle::current(),
            },
            events_rx,
        ))
    }

    pub fn mcp_url(&self) -> &str {
        &self.mcp_url
    }

    /// Live (unrevoked) chat tokens. For tests and diagnostics.
    pub fn live_tokens(&self) -> usize {
        self.tokens.len()
    }

    /// Resolve a pending provider permission with one of its advertised choices.
    pub fn resolve_approval(&self, id: &ApprovalId, choice: &str) -> Result<(), String> {
        self.approvals.resolve(id, choice)
    }

    /// Leave a pending provider permission unanswered, closing the provider path.
    pub fn cancel_approval(&self, id: &ApprovalId) -> Result<(), String> {
        self.approvals.cancel(id)
    }

    /// Most recently recorded handling outcome for a provider permission.
    pub fn approval_outcome(
        &self,
        id: &ApprovalId,
    ) -> Result<Option<crate::approval::ApprovalOutcome>, String> {
        self.approvals
            .outcomes
            .lock()
            .map(|outcomes| outcomes.get(id).cloned())
            .map_err(|_| "approval outcome lock poisoned".to_string())
    }

    /// Runs `f` on the engine inside the actor and returns its result.
    pub async fn with_engine<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Engine, Millis) -> R + Send + 'static,
    ) -> Result<R, String> {
        let (reply, wait) = oneshot::channel();
        let f: EngineFn = Box::new(move |engine, now| {
            let _ = reply.send(f(engine, now));
        });
        self.tx.send(Cmd::Engine(f)).map_err(|_| GONE.to_string())?;
        wait.await.map_err(|_| GONE.to_string())
    }

    /// Creates the project `id` or refreshes its display name and the folder
    /// it was last seen in (`root`). Deciding *which* id a folder gets, and
    /// asking the user when that changes, is the caller's job; this grants
    /// no one access to anything.
    pub async fn ensure_project(
        &self,
        id: ProjectId,
        name: &str,
        root: Option<&str>,
    ) -> Result<Project, String> {
        let (name, root) = (name.to_string(), root.map(str::to_string));
        self.with_engine(move |e, now| e.ensure_project(&id, &name, root.as_deref(), now))
            .await?
            .map_err(|e| e.to_string())
    }

    pub async fn project(&self, id: ProjectId) -> Result<Option<Project>, String> {
        self.with_engine(move |e, _| crate::store::project(e.conn(), &id))
            .await?
            .map_err(|e| e.to_string())
    }

    /// The project last opened from `root`, if any.
    pub async fn project_by_root(&self, root: &str) -> Result<Option<Project>, String> {
        let root = root.to_string();
        self.with_engine(move |e, _| crate::store::project_by_root(e.conn(), &root))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Every project, oldest first.
    pub async fn projects(&self) -> Result<Vec<Project>, String> {
        self.with_engine(|e, _| crate::store::projects(e.conn()))
            .await?
            .map_err(|e| e.to_string())
    }

    /// The migrated "Imported" project, while no folder has adopted it. Never
    /// a project with a folder.
    pub async fn unclaimed_import(&self) -> Result<Option<Project>, String> {
        self.with_engine(|e, _| crate::store::unclaimed_import(e.conn()))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Makes the unclaimed import `id` the project of the folder `root`,
    /// named `name`, once. Fails if `id` is not (or no longer) an unclaimed
    /// import without a folder. Grants no one anything.
    pub async fn adopt_import(
        &self,
        id: ProjectId,
        name: &str,
        root: &str,
    ) -> Result<Project, String> {
        let (name, root) = (name.to_string(), root.to_string());
        self.with_engine(move |e, _| e.adopt_import(&id, &name, &root))
            .await?
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "that history is no longer available to adopt".to_string())
    }

    /// Whether anything was ever said in `id`'s channels.
    pub async fn project_has_messages(&self, id: ProjectId) -> Result<bool, String> {
        self.with_engine(move |e, _| {
            let Ok(ws) = e.store().project_workspace(&id) else {
                return Ok(false);
            };
            ws.query_row("SELECT EXISTS(SELECT 1 FROM messages)", [], |r| r.get(0))
        })
        .await?
        .map_err(|e| e.to_string())
    }

    /// Deletes `id` if it has no folder, no messages and isn't the unclaimed
    /// import (see `store::forget_empty_project`). Returns whether it went.
    pub async fn forget_empty_project(&self, id: ProjectId) -> Result<bool, String> {
        self.with_engine(move |e, _| e.forget_empty_project(&id))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Renames a project. Its id, channels and history stay.
    pub async fn rename_project(&self, id: ProjectId, name: &str) -> Result<(), String> {
        let name = name.to_string();
        self.with_engine(move |e, _| crate::store::rename_project(e.conn(), &id, &name))
            .await?
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The channel `name` in `project`, created if new. Channel names are
    /// only unique within a project.
    pub async fn channel(&self, project: &ProjectId, name: &str) -> Result<Channel, String> {
        let (project, name) = (project.clone(), name.to_string());
        self.with_engine(move |e, now| e.ensure_channel(&project, &name, now))
            .await?
            .map_err(|e| e.to_string())
    }

    /// A project's channels, in `::N` order.
    pub async fn channels(&self, project: &ProjectId) -> Result<Vec<Channel>, String> {
        let project = project.clone();
        self.with_engine(move |e, _| e.store().channels_in_project(&project))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Every project with its channels, oldest project first, each project's
    /// channels in `::N` order. The cross-project chat list is built from this
    /// one answer so a stale per-project loop cannot interleave (STRM-737).
    pub async fn project_catalog(&self) -> Result<Vec<(Project, Vec<Channel>)>, String> {
        self.with_engine(|e, _| e.store().project_catalog())
            .await?
            .map_err(|e| e.to_string())
    }

    /// Workspace-level chats (STRM-745). v1: one chat per channel.
    pub async fn chats(&self) -> Result<Vec<Chat>, String> {
        self.with_engine(|e, _| e.store().chats())
            .await?
            .map_err(|e| e.to_string())
    }

    /// Renames a channel. Its id, `::N` and history stay.
    pub async fn rename_channel(&self, channel: ChannelId, name: &str) -> Result<(), String> {
        let name = name.to_string();
        self.with_engine(move |e, _| e.store().rename_channel(channel, &name))
            .await?
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Grants `member` membership of `project` (the permission to join its
    /// channels).
    pub async fn join_project(&self, project: &ProjectId, member: MemberId) -> Result<(), String> {
        let project = project.clone();
        self.with_engine(move |e, now| e.join_project(&project, member, now))
            .await?
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Joins `member` to `channel`. Refused (`not_a_project_member`) unless
    /// the member already belongs to the channel's project.
    pub async fn join(&self, member: MemberId, channel: ChannelId) -> Result<(), String> {
        self.with_engine(move |e, now| {
            let m = crate::store::member(e.conn(), member)?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            e.join(channel, &m, now)
        })
        .await?
        .map_err(|e| e.to_string())
    }

    /// A human member, created if new, made a member of `channel`'s project
    /// and joined to `channel`. Calling this is the host granting that
    /// person the project (the person at the keyboard, for the app).
    pub async fn human(
        &self,
        handle: &str,
        name: &str,
        channel: ChannelId,
    ) -> Result<Member, String> {
        let (handle, name) = (handle.to_string(), name.to_string());
        self.with_engine(move |e, now| {
            let m = e.ensure_member(MemberKind::Human, &handle, &name)?;
            let ch = e
                .store()
                .channel(channel)?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            e.join_project(&ch.project, m.id, now)?;
            e.join(channel, &m, now)?;
            Ok::<_, rusqlite::Error>(m)
        })
        .await?
        .map_err(|e| e.to_string())
    }

    /// Adds an agent to `channel` (joins it with the backfill cursor and runs
    /// the adapter's probe). No session starts until the agent is woken.
    ///
    /// This is the host's explicit grant: the agent also becomes a member of
    /// the channel's project. Nothing an agent does can reach this path.
    pub async fn add_agent(&self, spec: AgentSpec, channel: ChannelId) -> Result<Member, String> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(Cmd::AddAgent {
                spec,
                channel,
                reply,
            })
            .map_err(|_| GONE.to_string())?;
        wait.await.map_err(|_| GONE.to_string())?
    }

    /// Adds a model member to `channel`'s project roster and joins it to
    /// `channel`, like [`Core::add_agent`]. Idempotent: the same harness and
    /// model is always the same member, so this also re-registers roster
    /// members at startup. One harness can host many members (one adapter
    /// each).
    pub async fn add_model_member(
        &self,
        spec: MemberSpec,
        channel: ChannelId,
    ) -> Result<crate::roster::ModelMember, String> {
        let (harness, model, name) = (
            spec.harness.clone(),
            spec.model.clone(),
            spec.model_name.clone(),
        );
        let mm = self
            .with_engine(move |e, now| {
                let project = e
                    .store()
                    .channel(channel)?
                    .ok_or(rusqlite::Error::QueryReturnedNoRows)?
                    .project;
                let core = e.conn();
                let mm =
                    crate::roster::ensure_model_member(core, &harness, model.as_deref(), &name)?;
                crate::roster::roster_add(core, &project, mm.member.id, now)?;
                Ok::<_, rusqlite::Error>(mm)
            })
            .await?
            .map_err(|e| e.to_string())?;
        self.add_agent(
            AgentSpec {
                handle: mm.member.handle.to_string(),
                name: mm.model_name.to_string(),
                adapter: spec.adapter,
                cwd: spec.cwd,
                ping_answers: None,
            },
            channel,
        )
        .await?;
        Ok(mm)
    }

    /// Takes a member off `project`'s roster and out of its channels; any
    /// session it had there is interrupted and closed, and anything still
    /// owed to it is dropped (see `roster::leave_project`). Its messages
    /// stay.
    pub async fn remove_member(&self, project: &ProjectId, member: MemberId) -> Result<(), String> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(Cmd::RemoveMember {
                project: project.clone(),
                member,
                reply,
            })
            .map_err(|_| GONE.to_string())?;
        wait.await.map_err(|_| GONE.to_string())?
    }

    /// The project's model members, in the order they were added.
    pub async fn roster(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<crate::roster::ModelMember>, String> {
        let project = project.clone();
        self.with_engine(move |e, _| crate::roster::roster(e.conn(), &project))
            .await?
            .map_err(|e| e.to_string())
    }

    /// True the first time it is asked for `project`: the caller then fills
    /// the roster from auto-detection. Afterwards the roster is the user's.
    pub async fn seed_roster_once(&self, project: &ProjectId) -> Result<bool, String> {
        let project = project.clone();
        self.with_engine(move |e, now| e.store_tx(|c| crate::roster::mark_seeded(c, &project, now)))
            .await?
            .map_err(|e| e.to_string())
    }

    /// The harness's cached model list (empty until it reported one).
    pub async fn harness_models(
        &self,
        harness: &str,
    ) -> Result<Vec<crate::harness::ModelInfo>, String> {
        let harness = harness.to_string();
        self.with_engine(move |e, _| crate::roster::harness_models(e.conn(), &harness))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Asks `adapter` for its harness's models (`AgentAdapter::list_models`)
    /// and caches a non-empty answer in the store. Spawns the harness once,
    /// on core's own runtime (the `rt` handle), so this is safe to await
    /// from any executor; a panic inside the adapter comes back as an error
    /// instead of taking the caller down. Callers run it in the background
    /// (at startup, or when a cached list is stale), never while the user
    /// waits.
    pub async fn discover_models(
        &self,
        harness: &str,
        adapter: Arc<dyn AgentAdapter>,
        cwd: PathBuf,
    ) -> Result<Vec<crate::harness::ModelInfo>, String> {
        let listed = self
            .rt
            .spawn(async move { adapter.list_models(cwd).await })
            .await
            .map_err(|e| {
                if e.is_panic() {
                    "the harness adapter panicked while listing models".to_string()
                } else {
                    "listing models was cancelled".to_string()
                }
            })?;
        let found = match listed {
            None => return Ok(Vec::new()),
            Some(r) => r.map_err(|e| e.to_string())?,
        };
        if !found.is_empty() {
            let (h, models) = (harness.to_string(), found.clone());
            self.with_engine(move |e, now| {
                e.store_tx(|c| crate::roster::set_harness_models(c, &h, &models, now))
            })
            .await?
            .map_err(|e| e.to_string())?;
        }
        Ok(found)
    }

    /// Probes `harness` for its models in the background (on core's runtime,
    /// like [`Core::discover_models`]) and writes a non-empty answer to the
    /// on-disk model cache in `cache_dir` under `version` (see
    /// `model_cache`). Returns what was cached.
    pub async fn probe_models(
        &self,
        harness: &str,
        version: &str,
        adapter: Arc<dyn AgentAdapter>,
        cwd: PathBuf,
        cache_dir: PathBuf,
    ) -> Result<crate::model_cache::CachedModels, String> {
        let found = self.discover_models(harness, adapter, cwd).await?;
        if found.is_empty() {
            return Err("it listed no models".to_string());
        }
        let cached = crate::model_cache::CachedModels::new(
            harness,
            version,
            crate::model_cache::now_secs(),
            &found,
        );
        let to_save = cached.clone();
        self.rt
            .spawn_blocking(move || crate::model_cache::save(&cache_dir, &to_save))
            .await
            .map_err(|_| "saving the model list was cancelled".to_string())?
            .map_err(|e| format!("could not save the model list: {e}"))?;
        Ok(cached)
    }

    /// Every agent member core has ever had, including removed ones (their
    /// messages stay, and read as a previous agent's).
    pub async fn agent_members(&self) -> Result<Vec<MemberId>, String> {
        self.with_engine(|e, _| crate::store::agent_member_ids(e.conn()))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Every `@token` of an agent that was in `project` and has been
    /// removed from it, as `(member, token)` (see
    /// [`crate::store::former_project_agent_mention_tokens`]).
    pub async fn former_project_agent_mention_tokens(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<(MemberId, Arc<str>)>, String> {
        let project = project.clone();
        self.with_engine(move |e, _| e.store().former_project_agent_mention_tokens(&project))
            .await?
            .map_err(|e| e.to_string())
    }

    pub async fn post(
        &self,
        author: MemberId,
        channel: ChannelId,
        body: &str,
        reply_to: Option<MessageId>,
    ) -> Result<Message, PostError> {
        let body = body.to_string();
        self.with_engine(move |e, now| {
            e.post(
                now,
                PostRequest {
                    author,
                    channel,
                    body: &body,
                    reply_to,
                    origin: Origin::Composer,
                    client_request_id: None,
                },
            )
            .map(|p| p.message)
        })
        .await
        .map_err(PostError::Store)?
    }

    /// Adds (`add`) or removes `member`'s reaction. Idempotent, and never a
    /// wake (see [`Engine::react`]).
    pub async fn react(
        &self,
        member: MemberId,
        message: MessageId,
        emoji: &str,
        add: bool,
    ) -> Result<Reacted, PostError> {
        let emoji = emoji.to_string();
        self.with_engine(move |e, now| e.react(now, member, message, &emoji, add))
            .await
            .map_err(PostError::Store)?
    }

    /// The project's list of reactions that answer a ping, as stored (`["*"]`
    /// or emoji), or `None` when it uses the default (✅, 👍).
    pub async fn ping_answer_reactions(
        &self,
        project: ProjectId,
    ) -> Result<Option<Vec<String>>, String> {
        self.with_engine(move |e, _| e.project_ping_answers(&project))
            .await?
            .map(|p| p.map(|p| p.to_list()))
            .map_err(|e| e.to_string())
    }

    /// Validates and saves the project's list of reactions that answer a
    /// ping (`None` goes back to the default). Junk is rejected whole with an
    /// error naming each bad entry. Returns the list now in effect.
    pub async fn set_ping_answer_reactions(
        &self,
        project: ProjectId,
        entries: Option<Vec<String>>,
    ) -> Result<Vec<String>, String> {
        self.with_engine(move |e, now| {
            e.set_project_ping_answers(now, &project, entries.as_deref())
                .map(|p| p.to_list())
                .map_err(|e| e.to_string())
        })
        .await?
    }

    /// `member`'s nicknames (STRM-560), oldest first. Each reaches the member
    /// with `@` exactly like its handle.
    pub async fn nicknames(&self, member: MemberId) -> Result<Vec<Arc<str>>, String> {
        self.with_engine(move |e, _| e.nicknames(member).map_err(|e| e.to_string()))
            .await?
    }

    /// Gives `member` a nickname. Refused (with a message saying why) when
    /// it is not a mention token, is @everyone/@here/@channel, or already
    /// reaches someone as a handle or nickname. Returns its nicknames now.
    pub async fn add_nickname(
        &self,
        member: MemberId,
        nickname: &str,
    ) -> Result<Vec<Arc<str>>, String> {
        let nickname = nickname.to_string();
        self.with_engine(move |e, now| {
            e.add_nickname(now, member, &nickname)
                .map_err(|e| e.to_string())
        })
        .await?
    }

    /// Takes a nickname away from `member`. Returns its nicknames now.
    pub async fn remove_nickname(
        &self,
        member: MemberId,
        nickname: &str,
    ) -> Result<Vec<Arc<str>>, String> {
        let nickname = nickname.to_string();
        self.with_engine(move |e, _| {
            e.remove_nickname(member, &nickname)
                .map_err(|e| e.to_string())
        })
        .await?
    }

    /// A roster model member's own list of reactions that answer a ping, as
    /// stored, or `None` when it follows the project setting.
    pub async fn member_ping_answer_reactions(
        &self,
        member: MemberId,
    ) -> Result<Option<Vec<String>>, String> {
        self.with_engine(move |e, _| e.member_ping_answers(member))
            .await?
            .map(|p| p.map(|p| p.to_list()))
            .map_err(|e| e.to_string())
    }

    /// Validates and saves a roster model member's own list of reactions
    /// that answer a ping (`None` clears it). It wins over the project
    /// setting; an `agents.json` list still wins over it. Returns the
    /// member's own list now (`None`: it follows the project).
    pub async fn set_member_ping_answer_reactions(
        &self,
        member: MemberId,
        entries: Option<Vec<String>>,
    ) -> Result<Option<Vec<String>>, String> {
        self.with_engine(move |e, _| {
            e.set_member_ping_answers(member, entries.as_deref())
                .map(|p| p.map(|p| p.to_list()))
                .map_err(|e| e.to_string())
        })
        .await?
    }

    /// Whether a roster model member receives normal (broadcast) messages
    /// (STRM-657). `None` when the agent has no preference stored.
    pub async fn member_wake_normal_messages(
        &self,
        member: MemberId,
    ) -> Result<Option<bool>, String> {
        self.with_engine(move |e, _| e.member_wake_normal_messages(member))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Sets whether a roster model member receives normal (broadcast)
    /// messages (STRM-657). Err when the agent is not a roster model member.
    pub async fn set_member_wake_normal_messages(
        &self,
        member: MemberId,
        enabled: bool,
    ) -> Result<(), String> {
        self.with_engine(move |e, _| {
            e.set_member_wake_normal_messages(member, enabled)
                .map_err(|e| e.to_string())
        })
        .await?
    }

    // --- Agent lifecycle (STRM-686) -------------------------------------

    /// Global lifecycle of an agent. With no stored state it is Sleeping.
    pub async fn agent_lifecycle(&self, member: MemberId) -> Result<LifecycleState, String> {
        self.with_engine(move |e, _| e.agent_lifecycle(member))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Set global lifecycle. `forced_sleep` records an explicit user action;
    /// activation clears it. Sleeping memberships are left untouched.
    pub async fn set_agent_lifecycle(
        &self,
        member: MemberId,
        state: LifecycleState,
        forced_sleep: bool,
    ) -> Result<(), String> {
        self.with_engine(move |e, _| e.set_agent_lifecycle(member, state, forced_sleep))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Whether an agent was put to sleep by an explicit user action.
    pub async fn agent_forced_sleep(&self, member: MemberId) -> Result<bool, String> {
        self.with_engine(move |e, _| e.agent_forced_sleep(member))
            .await?
            .map_err(|e| e.to_string())
    }

    /// One agent's participation state in one chat.
    pub async fn chat_membership_lifecycle(
        &self,
        chat: ChatId,
        member: MemberId,
    ) -> Result<MembershipState, String> {
        self.with_engine(move |e, _| e.chat_membership_lifecycle(chat, member))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Set one chat membership's state. The chat and agent must exist.
    pub async fn set_chat_membership_lifecycle(
        &self,
        chat: ChatId,
        member: MemberId,
        state: MembershipState,
        forced_sleep: bool,
    ) -> Result<(), String> {
        self.with_engine(move |e, _| {
            e.set_chat_membership_lifecycle(chat, member, state, forced_sleep)
        })
        .await?
        .map_err(|e| e.to_string())
    }

    /// Whether this chat membership was explicitly force-slept by the user.
    pub async fn chat_membership_forced_sleep(
        &self,
        chat: ChatId,
        member: MemberId,
    ) -> Result<bool, String> {
        self.with_engine(move |e, _| e.chat_membership_forced_sleep(chat, member))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Precedence check for a candidate wake, exposed for UI/tests.
    pub async fn effective_delivery(
        &self,
        member: MemberId,
        channel: ChannelId,
        reason: WakeReason,
    ) -> Result<DeliveryDecision, String> {
        self.with_engine(move |e, _| e.effective_delivery(member, channel, reason))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Reactions on the messages of `channel` after `after_seq`, oldest first
    /// (load them with the history they belong to).
    pub async fn reactions(
        &self,
        channel: ChannelId,
        after_seq: Seq,
    ) -> Result<Vec<Reaction>, String> {
        self.with_engine(move |e, _| e.store().reactions_after_seq(channel, after_seq))
            .await?
            .map_err(|e| e.to_string())
    }

    /// A human refuses or dissents from `target` (agents use the chat tools).
    /// Posted as a reply carrying `reason`, recorded as a [`Stance`].
    pub async fn stance(
        &self,
        member: MemberId,
        channel: ChannelId,
        kind: StanceKind,
        target: MessageId,
        reason: &str,
    ) -> Result<Message, PostError> {
        let reason = reason.to_string();
        self.with_engine(move |e, now| {
            e.take_stance(
                now,
                StanceRequest {
                    author: member,
                    channel,
                    kind,
                    target,
                    reason: &reason,
                    model: None,
                    client_request_id: None,
                },
            )
            .map(|p| p.message)
        })
        .await
        .map_err(PostError::Store)?
    }

    /// The stance a message states, if any (for drawing it).
    pub async fn stance_of(&self, message: MessageId) -> Result<Option<Stance>, String> {
        self.with_engine(move |e, _| e.store().stance(message).map_err(|e| e.to_string()))
            .await?
    }

    pub async fn history(&self, channel: ChannelId, limit: usize) -> Result<Vec<Message>, String> {
        self.with_engine(move |e, _| e.history(channel, limit))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Records that a human was shown `channel` through `through` (None: the
    /// channel head). Moves their read receipt forward, never back.
    pub async fn mark_read(
        &self,
        member: MemberId,
        channel: ChannelId,
        through: Option<Seq>,
    ) -> Result<(), String> {
        self.with_engine(move |e, now| {
            e.advance_read(now, member, channel, through.unwrap_or(Seq::MAX))
        })
        .await
    }

    /// Who has read how far in `channel` (see [`Receipt`]).
    pub async fn receipts(&self, channel: ChannelId) -> Result<Vec<Receipt>, String> {
        self.with_engine(move |e, _| e.receipts(channel))
            .await?
            .map_err(|e| e.to_string())
    }

    pub async fn members(&self, channel: ChannelId) -> Result<Vec<Member>, String> {
        self.with_engine(move |e, _| e.store().members_of(channel))
            .await?
            .map_err(|e| e.to_string())
    }

    pub async fn status(&self, agent: MemberId, channel: ChannelId) -> Result<AgentStatus, String> {
        self.with_engine(move |e, _| e.status(agent, channel)).await
    }

    /// Re-runs trigger creation for a message; a no-op for existing triggers.
    pub async fn retrigger(&self, message: MessageId) -> Result<(), String> {
        self.with_engine(move |e, now| e.retrigger(now, message))
            .await?
            .map_err(|e| e.to_string())
    }

    /// Calls a chat tool as the session `scope` would (tests, debugging).
    pub async fn tool(&self, scope: Scope, name: &str, args: Value) -> Result<Value, String> {
        let name = name.to_string();
        self.with_engine(move |e, now| mcp::call_tool(e, now, scope, &name, &args).map_err(|e| e.0))
            .await?
    }

    pub fn interrupt(&self, agent: MemberId, channel: ChannelId) {
        let _ = self.tx.send(Cmd::Interrupt { agent, channel });
    }

    /// Channels the control applies to. Global covers every channel the agent
    /// has joined; Chat covers only that chat's targets.
    async fn channels_for_scope(
        &self,
        member: MemberId,
        scope: &LifecycleScope,
    ) -> Result<Vec<ChannelId>, String> {
        let scope = scope.clone();
        self.with_engine(move |e, _| {
            let is_agent =
                matches!(e.store().member(member), Ok(Some(m)) if m.kind == MemberKind::Agent);
            if !is_agent {
                return Err("no such agent".to_string());
            }
            match scope {
                LifecycleScope::Global => {
                    let mut channels = Vec::new();
                    for project in e.store().projects().map_err(|e| e.to_string())? {
                        channels.extend(
                            e.store()
                                .channels_of(member, &project.id)
                                .map_err(|e| e.to_string())?
                                .into_iter()
                                .map(|c| c.id),
                        );
                    }
                    Ok(channels)
                }
                LifecycleScope::Chat(chat) => {
                    let targets = e.store().chat_targets(chat).map_err(|e| e.to_string())?;
                    if targets.is_empty() {
                        return Err("no such chat".to_string());
                    }
                    Ok(targets.into_iter().map(|t| t.channel).collect())
                }
            }
        })
        .await?
    }

    async fn interrupt_scoped(
        &self,
        agent: MemberId,
        channels: Vec<ChannelId>,
        fallback_close: bool,
    ) -> Result<Vec<(ChannelId, InterruptOutcome)>, String> {
        if channels.is_empty() {
            return Ok(Vec::new());
        }
        let (reply, wait) = oneshot::channel();
        if self
            .tx
            .send(Cmd::InterruptScoped {
                agent,
                channels,
                fallback_close,
                reply,
            })
            .is_err()
        {
            return Err("runtime stopped".to_string());
        }
        wait.await.map_err(|_| "runtime stopped".to_string())
    }

    /// High-priority instruction straight to a live session. It creates no
    /// message, batch or obligation, and its acceptance is not proof of
    /// compliance.
    async fn deliver_instruction(
        &self,
        agent: MemberId,
        channel: ChannelId,
        prompt: Arc<str>,
    ) -> Result<(), String> {
        let (reply, wait) = oneshot::channel();
        if self
            .tx
            .send(Cmd::DeliverInstruction {
                agent,
                channel,
                prompt,
                reply,
            })
            .is_err()
        {
            return Err("runtime stopped".to_string());
        }
        wait.await.map_err(|_| "runtime stopped".to_string())?
    }

    /// A confirmed cancel ended the turn; settle it even if the adapter drops
    /// the late response, so pending work is not stranded behind a dead batch.
    /// Unsupported/failed results are left alone: the turn may still run.
    async fn settle_confirmed(
        &self,
        agent: MemberId,
        interruptions: &[(ChannelId, InterruptOutcome)],
    ) -> Result<(), String> {
        let confirmed: Vec<ChannelId> = interruptions
            .iter()
            .filter(|(_, outcome)| *outcome == InterruptOutcome::Confirmed)
            .map(|(channel, _)| *channel)
            .collect();
        if confirmed.is_empty() {
            return Ok(());
        }
        self.with_engine(move |e, now| {
            for channel in confirmed {
                e.settle_interrupted(now, agent, channel);
            }
        })
        .await
    }

    /// Stop: interrupt the running turn at the selected scope. Wake preference
    /// and lifecycle state are unchanged. The report says per channel whether
    /// cancellation was confirmed, unsupported, failed, or fell back to
    /// closing the owned session. Completed side effects are never undone.
    pub async fn stop(
        &self,
        member: MemberId,
        scope: LifecycleScope,
    ) -> Result<ControlOutcome, String> {
        let channels = self.channels_for_scope(member, &scope).await?;
        let interruptions = self.interrupt_scoped(member, channels, true).await?;
        self.settle_confirmed(member, &interruptions).await?;
        Ok(ControlOutcome {
            interruptions,
            sleep: None,
        })
    }

    /// Force sleep: interrupt first, then persist Sleeping at the selected
    /// scope. A user-forced sleep beats the agent's wake preference and is not
    /// cleared by global activation.
    pub async fn force_sleep(
        &self,
        member: MemberId,
        scope: LifecycleScope,
    ) -> Result<ControlOutcome, String> {
        let channels = self.channels_for_scope(member, &scope).await?;
        let interruptions = self.interrupt_scoped(member, channels, true).await?;
        let sleep = match scope {
            LifecycleScope::Global => {
                self.set_agent_lifecycle(member, LifecycleState::Sleeping, true)
                    .await?;
                SleepApplied::Global(LifecycleState::Sleeping)
            }
            LifecycleScope::Chat(chat) => {
                self.set_chat_membership_lifecycle(chat, member, MembershipState::Sleeping, true)
                    .await?;
                SleepApplied::Chat(chat, MembershipState::Sleeping)
            }
        };
        // Settle after the sleep is persisted: a confirmed cancel must not
        // let pending work pump into a freshly opened turn before the wake
        // preference is written.
        self.settle_confirmed(member, &interruptions).await?;
        Ok(ControlOutcome {
            interruptions,
            sleep: Some(sleep),
        })
    }

    /// Last Stop-and-check-chat instruction for an agent in a chat/channel.
    /// `instruction_delivered` is not a claim that the agent read anything.
    pub async fn context_refresh(
        &self,
        member: MemberId,
        channel: ChannelId,
    ) -> Result<Option<ContextRefresh>, String> {
        self.with_engine(move |e, _| e.context_refresh(member, channel).cloned())
            .await
    }

    /// Stop and check chat: interrupt first, then hand a high-priority
    /// instruction to every live session in scope telling it to inspect the
    /// unread part of that chat before doing anything else. The instruction is
    /// not a chat message, creates no batch or obligation, and delivery is not
    /// proof of compliance. Pending work stays pending, so a resume cannot lose
    /// or duplicate an obligation.
    pub async fn stop_and_check_chat(
        &self,
        member: MemberId,
        scope: LifecycleScope,
    ) -> Result<RefreshOutcome, String> {
        let channels = self.channels_for_scope(member, &scope).await?;
        // Do not close the session as a fallback here: we still need it to
        // receive the instruction. Unsupported/failed interrupts are reported.
        let interruptions = self
            .interrupt_scoped(member, channels.clone(), false)
            .await?;

        let points = self
            .with_engine(move |e, now| {
                let mut points = Vec::new();
                for channel in channels {
                    let Some(project) = e.store().project_for_channel(channel) else {
                        continue;
                    };
                    let chats = e
                        .store()
                        .chats_for_channel(&project, channel)
                        .map_err(|e| e.to_string())?;
                    let Some(chat) = chats.into_iter().next() else {
                        continue;
                    };
                    let cursor = e
                        .store()
                        .cursor(member, channel)
                        .map_err(|e| e.to_string())?;
                    let head = e.store().head_seq(channel).map_err(|e| e.to_string())?;
                    points.push((now, chat, channel, cursor.read_through, head));
                }
                Ok::<_, String>(points)
            })
            .await??;

        let mut instructed = Vec::new();
        let mut no_session = Vec::new();
        for (now, chat, channel, read_through, head) in points {
            let interrupted = interruptions
                .iter()
                .find(|(c, _)| *c == channel)
                .map(|(_, o)| o);
            match interrupted {
                Some(InterruptOutcome::Confirmed) => {}
                Some(InterruptOutcome::NoActiveTurn) | None => {
                    no_session.push(chat);
                    continue;
                }
                // A rejected or unsupported cancel leaves the turn running.
                // Do not prompt the same session again or settle its batch:
                // engine state and the live session must agree on which turn
                // is active. The failure stays visible in `interruptions`.
                Some(_) => continue,
            }
            let unread = head.saturating_sub(read_through);
            let prompt: Arc<str> = Arc::from(format!(
                "Chaos Control runtime instruction (high priority, not a chat message). \
                 Stop your current work. Before doing anything else, inspect this chat's \
                 conversation from sequence {} through {} ({} messages past your last read point) \
                 so your context is current, then continue from that context. \
                 Delivery of this instruction does not prove you have read it.",
                read_through.saturating_add(1),
                head,
                unread
            ));
            let delivered = self
                .deliver_instruction(member, channel, prompt)
                .await
                .is_ok();
            // The cancel notification was sent before the instruction. Some
            // adapters track one turn at a time and can drop the cancelled
            // turn's late response, so settle it here instead of waiting for
            // that response; otherwise pending work would never resume.
            self.with_engine(move |e, now| {
                e.settle_interrupted(now, member, channel);
            })
            .await?;
            let refresh = ContextRefresh {
                requested_at: now,
                read_through,
                head_seq: head,
                instruction_delivered: delivered,
            };
            if delivered {
                instructed.push((chat, channel, unread));
            }
            self.with_engine(move |e, _| {
                e.record_context_refresh(member, channel, refresh);
            })
            .await?;
        }

        Ok(RefreshOutcome {
            interruptions,
            instructed,
            no_session,
        })
    }

    /// Closes every session and stops the actor.
    pub async fn shutdown(&self) {
        let (reply, wait) = oneshot::channel();
        if self.tx.send(Cmd::Shutdown(reply)).is_ok() {
            let _ = wait.await;
        }
    }
}

// ---------- actor ----------

struct AgentEntry {
    handle: Arc<str>,
    adapter: Arc<dyn AgentAdapter>,
    cwd: PathBuf,
}

enum SlotState {
    Opening {
        queued: Option<Dispatch>,
    },
    Ready {
        session: Arc<dyn AgentSession>,
        fresh: bool,
    },
}

struct Slot {
    epoch: u64,
    token: String,
    state: SlotState,
}

struct Actor {
    engine: Engine,
    clock: Arc<dyn Clock>,
    tokens: Arc<Tokens>,
    mcp_url: String,
    bridge: Option<BridgeCommand>,
    default_cwd: PathBuf,
    agents: HashMap<MemberId, AgentEntry>,
    slots: HashMap<(MemberId, ChannelId), Slot>,
    approvals: Arc<RuntimeApprovalBroker>,
    next_epoch: u64,
    tx: mpsc::UnboundedSender<Cmd>,
    events: mpsc::UnboundedSender<CoreEvent>,
    alive: bool,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        while self.alive {
            let sleep = match self.engine.next_deadline() {
                Some(t) => self.clock.sleep_until(t),
                None => Box::pin(futures::future::pending()),
            };
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => break,
                },
                _ = sleep => {
                    let now = self.clock.now();
                    self.engine.tick(now);
                }
            }

            self.apply_effects();
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        let now = self.clock.now();
        match cmd {
            Cmd::Engine(f) => f(&mut self.engine, now),
            Cmd::AddAgent {
                spec,
                channel,
                reply,
            } => {
                let r = self.add_agent(spec, channel, now);
                let _ = reply.send(r);
            }
            Cmd::RemoveMember {
                project,
                member,
                reply,
            } => {
                let r = self.remove_member(&project, member, now);
                let _ = reply.send(r);
            }
            Cmd::Opened {
                agent,
                channel,
                epoch,
                result,
            } => self.opened(agent, channel, epoch, result, now),
            Cmd::Event {
                agent,
                channel,
                epoch,
                event,
            } => {
                if self.slots.get(&(agent, channel)).map(|s| s.epoch) != Some(epoch) {
                    return; // from a session that is already gone
                }
                let exited = matches!(event, AgentEvent::Exited { .. });
                self.engine.agent_event(now, agent, channel, event);
                if exited && let Some(slot) = self.slots.remove(&(agent, channel)) {
                    self.tokens.revoke(&slot.token);
                }
            }
            Cmd::SlotClosed {
                agent,
                channel,
                epoch,
            } => {
                if self.slots.get(&(agent, channel)).map(|s| s.epoch) == Some(epoch) {
                    if let Some(slot) = self.slots.remove(&(agent, channel)) {
                        self.tokens.revoke(&slot.token);
                    }
                    self.engine.session_closed(agent, channel);
                    // Closing the owned session killed the turn even when the
                    // adapter could not confirm a cancel. Settle it so pending
                    // work can run on a fresh session; completed side effects
                    // are not undone by this bookkeeping.
                    self.engine.settle_interrupted(now, agent, channel);
                }
            }
            Cmd::DeliverFailed {
                agent,
                channel,
                epoch,
                batch,
                error,
            } => {
                if self.slots.get(&(agent, channel)).map(|s| s.epoch) == Some(epoch) {
                    self.engine.delivery_failed(
                        now,
                        agent,
                        channel,
                        batch,
                        error.kind,
                        &error.message,
                    );
                }
            }
            Cmd::Interrupt { agent, channel } => {
                if let Some(Slot {
                    state: SlotState::Ready { session, .. },
                    ..
                }) = self.slots.get(&(agent, channel))
                {
                    let session = session.clone();
                    tokio::spawn(async move {
                        let _ = session.interrupt().await;
                    });
                }
            }
            Cmd::InterruptScoped {
                agent,
                channels,
                fallback_close,
                reply,
            } => {
                // One slot per input channel keeps the report in the caller's
                // order even when cancellations complete out of order.
                let tx = self.tx.clone();
                let mut outcomes: Vec<Option<(ChannelId, InterruptOutcome)>> =
                    channels.iter().map(|_| None).collect();
                let mut targets = Vec::new();
                for (index, channel) in channels.iter().enumerate() {
                    let channel = *channel;
                    match self.slots.get(&(agent, channel)) {
                        None => outcomes[index] = Some((channel, InterruptOutcome::NoActiveTurn)),
                        Some(Slot {
                            epoch,
                            state: SlotState::Ready { session, .. },
                            ..
                        }) => {
                            let epoch = *epoch;
                            if !session.info().caps.contains(Caps::INTERRUPT) {
                                if fallback_close {
                                    // Best effort: closing reports no result, so
                                    // say exactly that rather than claiming a
                                    // confirmed cancellation.
                                    let session = session.clone();
                                    let tx = tx.clone();
                                    tokio::spawn(async move {
                                        session.close().await;
                                        let _ = tx.send(Cmd::SlotClosed {
                                            agent,
                                            channel,
                                            epoch,
                                        });
                                    });
                                    outcomes[index] = Some((
                                        channel,
                                        InterruptOutcome::FallbackClosed {
                                            reason: "interrupt unsupported by session capabilities"
                                                .into(),
                                        },
                                    ));
                                } else {
                                    outcomes[index] =
                                        Some((channel, InterruptOutcome::Unsupported));
                                }
                                continue;
                            }
                            targets.push((index, channel, epoch, session.clone()));
                        }
                        Some(_) => {
                            outcomes[index] = Some((channel, InterruptOutcome::NoActiveTurn));
                        }
                    }
                }
                // Do not hold the actor while integrations answer. A late
                // result from the interrupted turn still carries the old
                // epoch, which `Cmd::Event` drops, so it cannot look like a
                // new turn or overwrite newer state.
                tokio::spawn(async move {
                    for (index, channel, epoch, session) in targets {
                        let outcome = match session.interrupt().await {
                            Ok(()) => InterruptOutcome::Confirmed,
                            Err(e) => InterruptOutcome::Failed(e.message.to_string()),
                        };
                        let outcome =
                            if fallback_close && !matches!(outcome, InterruptOutcome::Confirmed) {
                                session.close().await;
                                let _ = tx.send(Cmd::SlotClosed {
                                    agent,
                                    channel,
                                    epoch,
                                });
                                InterruptOutcome::FallbackClosed {
                                    reason: match outcome {
                                        InterruptOutcome::Unsupported => {
                                            "interrupt unsupported by session capabilities".into()
                                        }
                                        InterruptOutcome::Failed(e) => {
                                            format!("interrupt failed: {e}")
                                        }
                                        other => format!("{other:?}"),
                                    },
                                }
                            } else {
                                outcome
                            };
                        outcomes[index] = Some((channel, outcome));
                    }
                    let _ = reply.send(outcomes.into_iter().flatten().collect());
                });
            }
            Cmd::DeliverInstruction {
                agent,
                channel,
                prompt,
                reply,
            } => {
                let session = match self.slots.get(&(agent, channel)) {
                    Some(Slot {
                        state: SlotState::Ready { session, .. },
                        ..
                    }) => session.clone(),
                    _ => {
                        let _ = reply.send(Err("no live session".to_string()));
                        return;
                    }
                };
                // An instruction is not a batch: use a negative id that can
                // never collide with a stored batch. The engine ignores events
                // for unknown turns, so this cannot create or settle an
                // obligation.
                let turn = BatchId(-1);
                tokio::spawn(async move {
                    let r = session
                        .deliver(turn, prompt)
                        .await
                        .map_err(|e| e.message.to_string());
                    let _ = reply.send(r);
                });
            }
            Cmd::Shutdown(reply) => {
                for (_, slot) in self.slots.drain() {
                    self.tokens.revoke(&slot.token);
                    if let SlotState::Ready { session, .. } = slot.state {
                        tokio::spawn(async move { session.close().await });
                    }
                }
                self.alive = false;
                let _ = reply.send(());
            }
        }
    }

    fn add_agent(
        &mut self,
        spec: AgentSpec,
        channel: ChannelId,
        now: Millis,
    ) -> Result<Member, String> {
        let member = self
            .engine
            .ensure_member(MemberKind::Agent, &spec.handle, &spec.name)
            .map_err(|e| e.to_string())?;
        let project = self
            .engine
            .store()
            .channel(channel)
            .map_err(|e| e.to_string())?
            .ok_or("no such channel")?
            .project;
        self.engine
            .join_project(&project, member.id, now)
            .map_err(|e| e.to_string())?;
        self.engine
            .join(channel, &member, now)
            .map_err(|e| e.to_string())?;
        self.engine
            .set_agent_ping_answers(member.id, spec.ping_answers.clone());
        let agent_cwd = spec.cwd.unwrap_or_else(|| self.default_cwd.clone());
        let adapter = spec.adapter.clone();
        self.agents.insert(
            member.id,
            AgentEntry {
                handle: member.handle.clone(),
                adapter: spec.adapter,
                cwd: agent_cwd,
            },
        );
        let tx = self.tx.clone();
        let id = member.id;
        tokio::spawn(async move {
            let probe = adapter.probe().await;
            let f: EngineFn = Box::new(move |e, _| e.set_probe(id, channel, &probe));
            let _ = tx.send(Cmd::Engine(f));
        });
        Ok(member)
    }

    fn remove_member(
        &mut self,
        project: &ProjectId,
        member: MemberId,
        now: Millis,
    ) -> Result<(), String> {
        let channels = self
            .engine
            .leave_project(now, project, member)
            .map_err(|e| e.to_string())?;
        for ch in channels {
            if let Some(slot) = self.slots.remove(&(member, ch)) {
                self.tokens.revoke(&slot.token);
                if let SlotState::Ready { session, .. } = slot.state {
                    tokio::spawn(async move {
                        let _ = session.interrupt().await;
                        session.close().await;
                    });
                }
            }
        }
        let still_somewhere = crate::roster::project_count(self.engine.conn(), member)
            .map_err(|e| e.to_string())?
            > 0;
        if !still_somewhere {
            self.agents.remove(&member);
        }
        Ok(())
    }

    fn apply_effects(&mut self) {
        loop {
            let effects = self.engine.take_effects();
            if effects.is_empty() {
                break;
            }
            for effect in effects {
                match effect {
                    Effect::Event(e) => {
                        let _ = self.events.send(*e);
                    }
                    Effect::Dispatch(d) => self.dispatch(d),
                    Effect::Close { agent, channel } => {
                        if let Some(slot) = self.slots.remove(&(agent, channel)) {
                            self.tokens.revoke(&slot.token);
                            if let SlotState::Ready { session, .. } = slot.state {
                                tokio::spawn(async move { session.close().await });
                            }
                        }
                        self.engine.session_closed(agent, channel);
                    }
                }
            }
        }
    }

    fn dispatch(&mut self, d: Dispatch) {
        let key = (d.agent, d.channel);
        match self.slots.get_mut(&key) {
            Some(Slot {
                state: SlotState::Ready { session, fresh },
                epoch,
                ..
            }) => {
                let prompt = if *fresh {
                    Arc::from(format!("{}\n{}", d.orientation, d.prompt))
                } else {
                    d.prompt.clone()
                };
                *fresh = false;
                let (session, epoch) = (session.clone(), *epoch);
                self.deliver(session, epoch, d, prompt);
            }
            Some(Slot {
                state: SlotState::Opening { queued },
                ..
            }) => *queued = Some(d),
            None => self.open(d),
        }
    }

    fn deliver(&self, session: Arc<dyn AgentSession>, epoch: u64, d: Dispatch, prompt: Arc<str>) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Err(error) = session.deliver(d.batch, prompt).await {
                let _ = tx.send(Cmd::DeliverFailed {
                    agent: d.agent,
                    channel: d.channel,
                    epoch,
                    batch: d.batch,
                    error,
                });
            }
        });
    }

    fn open(&mut self, d: Dispatch) {
        let (agent, channel) = (d.agent, d.channel);
        let Some(entry) = self.agents.get(&agent) else {
            let now = self.clock.now();
            self.engine.delivery_failed(
                now,
                agent,
                channel,
                d.batch,
                AgentErrorKind::Spawn,
                "this agent is not configured in this run",
            );
            return;
        };
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        let token = self.tokens.mint(Scope {
            agent,
            channel,
            epoch,
        });
        let tx = self.tx.clone();
        let sink = EventSink::new(move |event| {
            let _ = tx.send(Cmd::Event {
                agent,
                channel,
                epoch,
                event,
            });
        });
        let req = OpenSession {
            agent,
            channel,
            handle: entry.handle.clone(),
            cwd: entry.cwd.clone(),
            chat_tools: ChatToolsEndpoint {
                url: self.mcp_url.clone(),
                token: token.clone(),
                bridge: self.bridge.clone(),
            },
            events: sink,
            approval_broker: Some(self.approvals.clone()),
        };
        let adapter = entry.adapter.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = adapter.open(req).await;
            let _ = tx.send(Cmd::Opened {
                agent,
                channel,
                epoch,
                result,
            });
        });
        self.slots.insert(
            (agent, channel),
            Slot {
                epoch,
                token,
                state: SlotState::Opening { queued: Some(d) },
            },
        );
    }

    fn opened(
        &mut self,
        agent: MemberId,
        channel: ChannelId,
        epoch: u64,
        result: Result<Arc<dyn AgentSession>, AgentError>,
        now: Millis,
    ) {
        let key = (agent, channel);
        if self.slots.get(&key).map(|s| s.epoch) != Some(epoch) {
            if let Ok(session) = result {
                tokio::spawn(async move { session.close().await });
            }
            return;
        }
        match result {
            Ok(session) => {
                self.engine
                    .session_opened(now, agent, channel, session.info());
                let slot = self.slots.get_mut(&key).expect("checked");
                let queued = match std::mem::replace(
                    &mut slot.state,
                    SlotState::Ready {
                        session: session.clone(),
                        fresh: true,
                    },
                ) {
                    SlotState::Opening { queued } => queued,
                    SlotState::Ready { .. } => None,
                };
                if let Some(d) = queued {
                    self.dispatch(d);
                }
            }
            Err(error) => {
                let slot = self.slots.remove(&key).expect("checked");
                self.tokens.revoke(&slot.token);
                if let SlotState::Opening { queued: Some(d) } = slot.state {
                    self.engine.delivery_failed(
                        now,
                        agent,
                        channel,
                        d.batch,
                        error.kind,
                        &error.message,
                    );
                }
                self.engine.session_closed(agent, channel);
            }
        }
    }
}
