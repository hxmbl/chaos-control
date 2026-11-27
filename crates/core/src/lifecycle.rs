//! Agent lifecycle rules (STRM-686).
//!
//! Lifecycle is deliberately separate from presence: working/idle and harness
//! activity say what an agent is doing right now; these states say whether it
//! may be woken at all. There are exactly three global states and two
//! per-chat membership states. Enforcement lives here as a pure function so
//! the engine, host APIs and UI all apply the same precedence.

use serde::{Deserialize, Serialize};

use crate::types::{ChannelId, ChatId, Millis, Seq, WakeReason};

/// Global lifecycle of one agent, across every chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    /// Available for normal room activity.
    Active,
    /// Master sit-and-wait state: no chat can wake or schedule the agent.
    Sleeping,
    /// Agent-wide failure. Not selectable as a sleep mode; blocks everything.
    Unresponsive,
}

impl LifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleState::Active => "active",
            LifecycleState::Sleeping => "sleeping",
            LifecycleState::Unresponsive => "unresponsive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(LifecycleState::Active),
            "sleeping" => Some(LifecycleState::Sleeping),
            "unresponsive" => Some(LifecycleState::Unresponsive),
            _ => None,
        }
    }

    /// New agents start globally Sleeping (STRM-670).
    pub fn default_for_new_agent() -> Self {
        LifecycleState::Sleeping
    }
}

/// Lifecycle of one agent's participation in one chat. Only meaningful while
/// the agent is globally Active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipState {
    Active,
    Sleeping,
}

impl MembershipState {
    pub fn as_str(self) -> &'static str {
        match self {
            MembershipState::Active => "active",
            MembershipState::Sleeping => "sleeping",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(MembershipState::Active),
            "sleeping" => Some(MembershipState::Sleeping),
            _ => None,
        }
    }

    /// Joining a chat does not put an otherwise-active agent to sleep there.
    pub fn default_for_new_membership() -> Self {
        MembershipState::Active
    }
}

/// Where a lifecycle control applies. Global acts on every chat; Chat acts
/// only on the memberships of that chat's targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleScope {
    Global,
    Chat(ChatId),
}

/// What the runtime can honestly report about an interruption. A confirmed
/// result means the live integration acknowledged cancellation; it never means
/// already-completed side effects were undone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterruptOutcome {
    /// The session confirmed the cancellation.
    Confirmed,
    /// There was no running turn to cancel.
    NoActiveTurn,
    /// The live integration does not advertise `Caps::INTERRUPT`.
    Unsupported,
    /// The integration tried and reported an error.
    Failed(String),
    /// Interruption was unsupported or failed; the owned session was closed
    /// as a best-effort fallback. Completed side effects are not undone.
    FallbackClosed { reason: String },
}

/// Lifecycle state actually written by a control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SleepApplied {
    Global(LifecycleState),
    Chat(ChatId, MembershipState),
}

/// Observable result of a Stop or Force sleep request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlOutcome {
    /// One entry per channel that had a session for this agent.
    pub interruptions: Vec<(ChannelId, InterruptOutcome)>,
    /// `None` for Stop; `Some` for Force sleep.
    pub sleep: Option<SleepApplied>,
}

impl ControlOutcome {
    /// True only when every target session confirmed cancellation (or there
    /// was nothing to cancel). A fallback close is not a confirmed cancel.
    pub fn all_interrupted(&self) -> bool {
        self.interruptions.iter().all(|(_, outcome)| {
            matches!(
                outcome,
                InterruptOutcome::Confirmed | InterruptOutcome::NoActiveTurn
            )
        })
    }
}

/// What the runtime knows about a "Stop and check chat" instruction. Delivery
/// of the instruction is deliberately not proof the agent read anything: the
/// only honest claim is that a live session accepted the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextRefresh {
    pub requested_at: Millis,
    /// The channel sequence the agent's cursor had read through.
    pub read_through: Seq,
    /// The channel head at the moment of the request.
    pub head_seq: Seq,
    /// Whether the live session accepted the high-priority instruction.
    pub instruction_delivered: bool,
}

/// Observable result of a Stop-and-check-chat request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshOutcome {
    pub interruptions: Vec<(ChannelId, InterruptOutcome)>,
    /// (chat, channel, unread message count) per instruction handed over.
    pub instructed: Vec<(ChatId, ChannelId, Seq)>,
    /// Chats with no live session, so no instruction could be delivered.
    pub no_session: Vec<ChatId>,
}

/// What the engine should do with a candidate wake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryDecision {
    /// Queue the trigger now.
    Deliver,
    /// Do not queue; leave lifecycle state unchanged.
    Skip,
    /// Flip a sleeping membership Active (explicit ping while globally Active)
    /// and queue the trigger. Global activation never does this.
    WakeMembership,
}

/// Apply Unresponsive > global Sleeping > chat Sleeping, then the explicit-ping
/// and wake-preference rules.
///
/// `preference_allows` is the agent-owned normal-message preference from
/// STRM-657. It only gates broadcasts: an explicit ping still wakes an agent
/// that has opted out of ordinary room chatter. A user-forced sleep is already
/// reflected in the state passed here; the preference cannot override it.
pub fn delivery_decision(
    global: LifecycleState,
    membership: MembershipState,
    reason: WakeReason,
    preference_allows: bool,
) -> DeliveryDecision {
    match global {
        LifecycleState::Unresponsive | LifecycleState::Sleeping => DeliveryDecision::Skip,
        LifecycleState::Active => match reason {
            WakeReason::Broadcast => {
                if !preference_allows || membership == MembershipState::Sleeping {
                    DeliveryDecision::Skip
                } else {
                    DeliveryDecision::Deliver
                }
            }
            WakeReason::Mention | WakeReason::ReplyToMe | WakeReason::Everyone => {
                if membership == MembershipState::Sleeping {
                    DeliveryDecision::WakeMembership
                } else {
                    DeliveryDecision::Deliver
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresponsive_blocks_everything() {
        for reason in [
            WakeReason::Broadcast,
            WakeReason::Mention,
            WakeReason::ReplyToMe,
            WakeReason::Everyone,
        ] {
            assert_eq!(
                delivery_decision(
                    LifecycleState::Unresponsive,
                    MembershipState::Active,
                    reason,
                    true
                ),
                DeliveryDecision::Skip
            );
        }
    }

    #[test]
    fn global_sleeping_blocks_everything_including_pings() {
        for reason in [
            WakeReason::Broadcast,
            WakeReason::Mention,
            WakeReason::ReplyToMe,
            WakeReason::Everyone,
        ] {
            assert_eq!(
                delivery_decision(
                    LifecycleState::Sleeping,
                    MembershipState::Active,
                    reason,
                    true
                ),
                DeliveryDecision::Skip
            );
        }
    }

    #[test]
    fn sleeping_membership_skips_broadcast_but_ping_wakes_it() {
        for reason in [
            WakeReason::Mention,
            WakeReason::ReplyToMe,
            WakeReason::Everyone,
        ] {
            assert_eq!(
                delivery_decision(
                    LifecycleState::Active,
                    MembershipState::Sleeping,
                    reason,
                    true
                ),
                DeliveryDecision::WakeMembership
            );
        }
        assert_eq!(
            delivery_decision(
                LifecycleState::Active,
                MembershipState::Sleeping,
                WakeReason::Broadcast,
                true
            ),
            DeliveryDecision::Skip
        );
    }

    #[test]
    fn preference_only_gates_broadcasts() {
        assert_eq!(
            delivery_decision(
                LifecycleState::Active,
                MembershipState::Active,
                WakeReason::Broadcast,
                false
            ),
            DeliveryDecision::Skip
        );
        assert_eq!(
            delivery_decision(
                LifecycleState::Active,
                MembershipState::Active,
                WakeReason::Mention,
                false
            ),
            DeliveryDecision::Deliver
        );
    }

    #[test]
    fn active_membership_delivers_broadcast() {
        assert_eq!(
            delivery_decision(
                LifecycleState::Active,
                MembershipState::Active,
                WakeReason::Broadcast,
                true
            ),
            DeliveryDecision::Deliver
        );
    }

    #[test]
    fn defaults_match_the_product_rules() {
        assert_eq!(
            LifecycleState::default_for_new_agent(),
            LifecycleState::Sleeping
        );
        assert_eq!(
            MembershipState::default_for_new_membership(),
            MembershipState::Active
        );
    }

    #[test]
    fn parsing_round_trips() {
        for state in [
            LifecycleState::Active,
            LifecycleState::Sleeping,
            LifecycleState::Unresponsive,
        ] {
            assert_eq!(LifecycleState::parse(state.as_str()), Some(state));
        }
        for state in [MembershipState::Active, MembershipState::Sleeping] {
            assert_eq!(MembershipState::parse(state.as_str()), Some(state));
        }
        assert_eq!(LifecycleState::parse("paused"), None);
        assert_eq!(MembershipState::parse("unresponsive"), None);
    }
}
