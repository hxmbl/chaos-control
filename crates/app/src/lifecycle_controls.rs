//! Agent lifecycle controls + harness-reported activity (STRM-689).
//!
//! Pure model for the discoverable, accessible control surface. The UI bridge
//! in [`crate::lifecycle_runtime`] turns a [`LifecycleCommand`] into a core
//! runtime call and reports the honest outcome; this module keeps the shared
//! vocabulary and never claims guaranteed termination or rollback. Delivery
//! policy is shared with `chaos_core::lifecycle` so the app cannot drift
//! from the runtime precedence rules.
//!
//! Contract (see chat #115 / STRM-670):
//! - Lifecycle: `Unresponsive > global Sleeping > chat Sleeping`.
//! - `Sleeping` members still wake on explicit pings.
//! - `Unresponsive` (error mode) surfaces failures (billing/auth, missing
//!   harness, repeated delivery failures); nothing is notified until an
//!   explicit ping, which makes the harness try again and may re-enter
//!   error mode.
//! - Harness-reported current activity is *presence*, separate from lifecycle.
//! - `Stop` = interrupt only; `Force sleep` = interrupt + state change;
//!   `Stop and check chat` = interrupt then high-priority context refresh.
//! - Present observable outcomes, cancellation failures, and unsupported
//!   provider capabilities honestly.

// Presentation helpers below are the shared contract for tests and the UI; a
// few are not constructed by the current app surface yet.
#![allow(dead_code)]

/// Which scope a lifecycle command applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleScope {
    /// Agent-wide lifecycle.
    Global,
    /// Per-chat membership lifecycle, keyed by chat id.
    Chat(String),
}

/// Agent-wide lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    Active,
    Sleeping,
    Unresponsive,
}

/// Per-chat membership lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MembershipState {
    Active,
    Sleeping,
}

/// Harness-reported current activity. Presence only; NOT lifecycle state.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum HarnessActivity {
    #[default]
    Idle,
    /// Free-text current activity reported by the harness, e.g. "running tests".
    Working(String),
}

/// Why an agent is `Unresponsive`. Surfaced, not hidden.
#[allow(dead_code)] // scaffold: variants surfaced once wired to runtime
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnresponsiveReason {
    BillingOrAuth,
    MissingHarness,
    RepeatedDeliveryFailure,
    Other(String),
}

/// A lifecycle command offered on a roster/card or command path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleCommand {
    /// Interrupt only; no state change.
    Stop,
    /// Interrupt + move to `Sleeping`.
    ForceSleep,
    /// Interrupt then high-priority context refresh (`check chat`).
    StopAndCheckChat,
}

/// A user intent: a command + the scope it targets. The runtime seam consumes
/// this; outcomes come back as [`Outcome`].
#[allow(dead_code)] // scaffold: constructed once wired to runtime
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleIntent {
    pub command: LifecycleCommand,
    pub scope: LifecycleScope,
}

/// Observable runtime outcome. Never claims guaranteed termination/rollback.
#[allow(dead_code)] // scaffold: constructed by runtime once wired
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Intent accepted; runtime requested.
    Requested,
    /// Interrupt delivered and observed.
    Interrupted,
    /// Cancellation requested but not confirmed; surface it, don't hide it.
    CancellationFailed(String),
    /// Provider/harness can't do this; say so.
    Unsupported(String),
    /// `Force sleep` applied a state change in `scope`.
    StateChanged(LifecycleScope),
    /// `Stop and check chat`: interrupt + refresh done / requested.
    ContextRefreshed,
}

/// Effective delivery precedence. Preference can only reduce, never override
/// forced `Sleeping`. Nothing is notified in `Sleeping` or `Unresponsive`
/// (error mode) except on an explicit ping; a ping wakes a sleeping member
/// (and a sleeping agent) and, in error mode, makes the harness try again.
pub fn effective_delivery(
    global: LifecycleState,
    membership: MembershipState,
    preference_allows_normal: bool,
    explicit_ping: bool,
) -> bool {
    let global = match global {
        LifecycleState::Active => chaos_core::lifecycle::LifecycleState::Active,
        LifecycleState::Sleeping => chaos_core::lifecycle::LifecycleState::Sleeping,
        LifecycleState::Unresponsive => chaos_core::lifecycle::LifecycleState::Unresponsive,
    };
    let membership = match membership {
        MembershipState::Active => chaos_core::lifecycle::MembershipState::Active,
        MembershipState::Sleeping => chaos_core::lifecycle::MembershipState::Sleeping,
    };
    let reason = if explicit_ping {
        chaos_core::types::WakeReason::Mention
    } else {
        chaos_core::types::WakeReason::Broadcast
    };
    matches!(
        chaos_core::lifecycle::delivery_decision(
            global,
            membership,
            reason,
            preference_allows_normal,
        ),
        chaos_core::lifecycle::DeliveryDecision::Deliver
            | chaos_core::lifecycle::DeliveryDecision::WakeMembership
            | chaos_core::lifecycle::DeliveryDecision::WakeAgent
            | chaos_core::lifecycle::DeliveryDecision::RetryUnresponsive
    )
}

/// Effective lifecycle for display, honoring precedence
/// `Unresponsive > global Sleeping > chat Sleeping`.
pub fn effective_lifecycle(global: LifecycleState, membership: MembershipState) -> LifecycleState {
    match global {
        LifecycleState::Unresponsive => LifecycleState::Unresponsive,
        LifecycleState::Sleeping => LifecycleState::Sleeping,
        LifecycleState::Active => match membership {
            MembershipState::Sleeping => LifecycleState::Sleeping,
            MembershipState::Active => LifecycleState::Active,
        },
    }
}

/// What `command` implies for state, independent of runtime success.
pub fn command_effects(command: LifecycleCommand, scope: &LifecycleScope) -> Vec<Outcome> {
    match command {
        LifecycleCommand::Stop => vec![Outcome::Requested],
        LifecycleCommand::ForceSleep => {
            vec![Outcome::Requested, Outcome::StateChanged(scope.clone())]
        }
        LifecycleCommand::StopAndCheckChat => vec![Outcome::Requested, Outcome::ContextRefreshed],
    }
}

/// Accessible name/label for a control. Keep scope explicit for screen readers.
pub fn accessible_label(command: LifecycleCommand, scope: &LifecycleScope, agent: &str) -> String {
    let verb = match command {
        LifecycleCommand::Stop => "Stop",
        LifecycleCommand::ForceSleep => "Force sleep",
        LifecycleCommand::StopAndCheckChat => "Stop and check chat",
    };
    let scope_text = match scope {
        LifecycleScope::Global => "agent-wide".to_string(),
        LifecycleScope::Chat(chat) => format!("in {chat}"),
    };
    format!("{verb} {agent} {scope_text}")
}

/// Short status text for presence, kept separate from lifecycle.
pub fn presence_label(activity: &HarnessActivity) -> String {
    match activity {
        HarnessActivity::Idle => "idle".to_string(),
        HarnessActivity::Working(text) => text.clone(),
    }
}

/// Status text for lifecycle; `Sleeping` notes explicit-ping wake.
pub fn lifecycle_label(state: LifecycleState, reason: Option<&UnresponsiveReason>) -> String {
    match state {
        LifecycleState::Active => "Active".to_string(),
        LifecycleState::Sleeping => "Sleeping — wakes on explicit ping".to_string(),
        LifecycleState::Unresponsive => match reason {
            Some(r) => format!("Unresponsive — {}", unresponsive_text(r)),
            None => "Unresponsive".to_string(),
        },
    }
}

fn unresponsive_text(reason: &UnresponsiveReason) -> String {
    match reason {
        UnresponsiveReason::BillingOrAuth => "billing/authentication problem".to_string(),
        UnresponsiveReason::MissingHarness => "missing harness".to_string(),
        UnresponsiveReason::RepeatedDeliveryFailure => "repeated delivery failures".to_string(),
        UnresponsiveReason::Other(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_unresponsive_beats_sleeping() {
        assert_eq!(
            effective_lifecycle(LifecycleState::Unresponsive, MembershipState::Active),
            LifecycleState::Unresponsive
        );
        assert_eq!(
            effective_lifecycle(LifecycleState::Sleeping, MembershipState::Active),
            LifecycleState::Sleeping
        );
        assert_eq!(
            effective_lifecycle(LifecycleState::Active, MembershipState::Sleeping),
            LifecycleState::Sleeping
        );
    }

    #[test]
    fn unresponsive_notifies_nothing_without_ping_but_ping_retries() {
        // No ping in error mode: nothing is notified.
        assert!(!effective_delivery(
            LifecycleState::Unresponsive,
            MembershipState::Active,
            true,
            false
        ));
        // A ping makes the harness try again.
        assert!(effective_delivery(
            LifecycleState::Unresponsive,
            MembershipState::Active,
            true,
            true
        ));
    }

    #[test]
    fn preference_cannot_override_forced_sleep() {
        // global active + membership sleeping + preference allowing => still blocked
        assert!(!effective_delivery(
            LifecycleState::Active,
            MembershipState::Sleeping,
            true,
            false
        ));
        // preference can only reduce normal delivery, not re-enable it
        assert!(!effective_delivery(
            LifecycleState::Active,
            MembershipState::Active,
            false,
            false
        ));
    }

    #[test]
    fn explicit_ping_wakes_sleeping_membership_and_sleeping_agent() {
        assert!(effective_delivery(
            LifecycleState::Active,
            MembershipState::Sleeping,
            false,
            true
        ));
        // A ping wakes a globally sleeping agent too.
        assert!(effective_delivery(
            LifecycleState::Sleeping,
            MembershipState::Active,
            true,
            true
        ));
        // But a global sleeping agent isn't notified on normal chat.
        assert!(!effective_delivery(
            LifecycleState::Sleeping,
            MembershipState::Active,
            true,
            false
        ));
    }

    #[test]
    fn stop_is_interrupt_only_no_state_change() {
        let fx = command_effects(LifecycleCommand::Stop, &LifecycleScope::Global);
        assert!(fx.contains(&Outcome::Requested));
        assert!(!fx.iter().any(|o| matches!(o, Outcome::StateChanged(_))));
    }

    #[test]
    fn force_sleep_interrupts_and_changes_state_in_scope() {
        let scope = LifecycleScope::Chat("main".into());
        let fx = command_effects(LifecycleCommand::ForceSleep, &scope);
        assert!(fx.contains(&Outcome::StateChanged(scope)));
    }

    #[test]
    fn stop_and_check_chat_refreshes_context() {
        let fx = command_effects(LifecycleCommand::StopAndCheckChat, &LifecycleScope::Global);
        assert!(fx.contains(&Outcome::ContextRefreshed));
    }

    #[test]
    fn accessible_label_names_verb_agent_scope() {
        let label = accessible_label(
            LifecycleCommand::ForceSleep,
            &LifecycleScope::Chat("main".into()),
            "@iceman-free",
        );
        assert_eq!(label, "Force sleep @iceman-free in main");
        let global = accessible_label(
            LifecycleCommand::Stop,
            &LifecycleScope::Global,
            "@big-pickle",
        );
        assert_eq!(global, "Stop @big-pickle agent-wide");
    }

    #[test]
    fn sleeping_mentions_explicit_ping_and_unresponsive_surfaces_reason() {
        assert!(lifecycle_label(LifecycleState::Sleeping, None).contains("explicit ping"));
        let s = lifecycle_label(
            LifecycleState::Unresponsive,
            Some(&UnresponsiveReason::BillingOrAuth),
        );
        assert!(s.contains("billing/authentication"));
    }

    #[test]
    fn presence_is_separate_from_lifecycle() {
        assert_eq!(presence_label(&HarnessActivity::Idle), "idle");
        assert_eq!(
            presence_label(&HarnessActivity::Working("running tests".into())),
            "running tests"
        );
    }
}
