//! Chaos Control-owned approval requests for mediated agent operations (STRM-612).
//!
//! This module defines the contract between an adapter/provider integration and
//! Chaos Control's approval authority. Adapters must not auto-approve requests
//! that require user approval, must not synthesize provider option/decision
//! semantics, and must treat timeout/disconnect/loss/ambiguity as fail-closed
//! (never approval, never replay).
//!
//! The initial consumer is the native OpenCode v2 adapter. Spike evidence
//! (v2.3.2, #248) shows v2 permission requests use a decision vocabulary
//! (`reject|once|always`) rather than provider option IDs; the types here are
//! shaped to carry verified provider choices without guessing semantics.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{BatchId, ChannelId, MemberId, Millis};

/// Chaos Control's stable identifier for one approval request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApprovalId(pub Arc<str>);

impl ApprovalId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ApprovalId {
    fn from(value: String) -> Self {
        ApprovalId(Arc::from(value))
    }
}

impl From<&str> for ApprovalId {
    fn from(value: &str) -> Self {
        ApprovalId(Arc::from(value))
    }
}

/// The provider that raised an approval request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalProvider {
    OpenCodeV2,
    #[serde(untagged)]
    Other(Arc<str>),
}

impl ApprovalProvider {
    pub fn as_str(&self) -> &str {
        match self {
            ApprovalProvider::OpenCodeV2 => "opencode_v2",
            ApprovalProvider::Other(value) => value,
        }
    }
}

/// A provider-supplied selectable option (ACP-style).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderOption {
    /// Provider option identifier; must be passed back verbatim when selected.
    pub id: Arc<str>,
    pub label: Option<Arc<str>>,
    /// Provider-specific kind/semantics label, if exposed.
    pub kind: Option<Arc<str>>,
    /// Raw provider payload for evidence/debugging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

/// Verified choices the user/CC may select for a request.
///
/// Adapters must not invent choices. For v2, the spike shows a fixed decision
/// vocabulary (`reject|once|always`) with no option IDs; represent that as a
/// vocabulary derived from provider evidence, not synthetic UI options.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalChoices {
    /// Provider supplied explicit selectable options (e.g. ACP permission options).
    ProviderOptions { options: Vec<ProviderOption> },
    /// Provider accepts a verified decision vocabulary/token set (e.g. v2).
    DecisionVocabulary {
        /// Allowed decision tokens, in provider-meaningful order when known.
        decisions: Vec<Arc<str>>,
        /// Raw provider payload describing choices/semantics, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
}

/// Operation details for an approval request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalOperation {
    /// Provider action/category (e.g. v2 `action`: `shell`, `read`).
    pub action: Arc<str>,
    pub tool_name: Option<Arc<str>>,
    pub summary: Option<Arc<str>>,
    /// Exact operation arguments/input when known. For v2, may require
    /// correlation with tool-call events keyed by permission `source.id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
    /// Raw provider payload for evidence (e.g. full `permission.asked` data).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

/// A CC-owned approval request raised by an adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub cc_request_id: ApprovalId,
    pub provider: ApprovalProvider,
    /// Provider's own request reference/id required to reply (e.g. v2 permission id).
    pub provider_request_ref: Arc<str>,
    /// Provider/native session identifier (e.g. v2 sessionID).
    pub session_id: Arc<str>,
    pub agent: MemberId,
    pub channel: ChannelId,
    /// The turn/batch this request belongs to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<BatchId>,
    pub operation: ApprovalOperation,
    pub choices: ApprovalChoices,
    pub requested_at: Millis,
    /// Additional correlation/context (e.g. v2 `source`, child/subagent origin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

/// Why an approval could not produce a provider reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolution {
    Lost,
    Timeout,
    Disconnect,
    Cancelled,
}

/// The decision CC returns to the adapter for one request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalDecisionKind {
    /// A provider-honored choice, passed back verbatim in provider-expected form
    /// (option id for ACP-style providers, decision token for v2).
    Selected { choice: Arc<str> },
    /// No provider reply may be sent; adapter must fail closed without replay.
    NotResolved { resolution: ApprovalResolution },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecision {
    pub cc_request_id: ApprovalId,
    pub decision: ApprovalDecisionKind,
    pub decided_at: Millis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<Arc<str>>,
}

/// Final/outcome state recorded for an approval request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcomeState {
    /// Provider reply sent indicating allow/once/always (or equivalent allow choice).
    Allowed,
    /// Provider reply sent indicating deny/reject/decline.
    Denied,
    /// Provider reply sent but request/operation failed or outcome is unknown-failed.
    Sent,
    /// No provider reply was sent (fail-closed); operation must not be treated as approved.
    NotSent,
    /// Request handling failed (e.g. transport/protocol error) without a safe approval.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalOutcome {
    pub cc_request_id: ApprovalId,
    pub state: ApprovalOutcomeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_reply: Option<Value>,
    /// Best-known execution effect (e.g. ran/blocked/error), when observable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_effect: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<Millis>,
}

/// Broker/authority that owns approval request state and user decisions.
///
/// Adapters call this to request and resolve provider permission prompts.
/// Implementations must enforce one-shot decisions per request, correlation, and
/// fail-closed behavior for unresolved/lost requests.
pub trait ApprovalBroker: Send + Sync {
    /// Register a request and await a decision or fail-closed resolution.
    ///
    /// The returned decision must be idempotent for the same `cc_request_id`.
    fn request_decision(
        &self,
        request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalDecision, ApprovalBrokerError>>;

    /// Record the outcome after attempting (or refusing) to reply/execute.
    fn record_outcome(
        &self,
        outcome: ApprovalOutcome,
    ) -> BoxFuture<'_, Result<(), ApprovalBrokerError>>;

    /// Fail closed all requests unresolved by this broker on transport loss.
    fn cancel_pending(
        &self,
        _resolution: ApprovalResolution,
    ) -> BoxFuture<'_, Result<(), ApprovalBrokerError>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalBrokerError {
    pub message: Arc<str>,
}

impl ApprovalBrokerError {
    pub fn new(message: impl Into<Arc<str>>) -> Self {
        ApprovalBrokerError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ApprovalBrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ApprovalBrokerError {}
