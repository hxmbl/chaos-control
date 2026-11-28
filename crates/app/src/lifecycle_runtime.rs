//! App-side bridge from lifecycle control intents to the core runtime.
//!
//! The pure model lives in [`crate::lifecycle_controls`]; this module owns the
//! only code that calls core's scoped control APIs and turns their honest
//! outcomes into short chat notes. Nothing here claims that an interrupted
//! agent's completed side effects were undone.

use chaos_core::{
    host::Core,
    lifecycle::{InterruptOutcome, LifecycleScope as CoreScope, RefreshOutcome, SleepApplied},
    types::{ChannelId, ChatId, MemberId},
};

use crate::lifecycle_controls::LifecycleCommand;

/// Where a control applies, as chosen in the app. The core `ChatId` is
/// resolved from the live chat at call time; `Chat` here only says which scope
/// the user picked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlScope {
    Global,
    Chat,
}

/// Runs one lifecycle command and returns the note to show in chat.
pub async fn run(
    core: &Core,
    member: MemberId,
    chat: Option<ChatId>,
    command: LifecycleCommand,
    scope: ControlScope,
) -> Result<String, String> {
    let core_scope = match scope {
        ControlScope::Global => CoreScope::Global,
        ControlScope::Chat => CoreScope::Chat(chat.ok_or("this chat has no core chat id")?),
    };
    match command {
        LifecycleCommand::Stop => {
            let outcome = core.stop(member, core_scope).await?;
            Ok(format!(
                "Stop: {}. Completed work is not undone.",
                interruptions(&outcome.interruptions)
            ))
        }
        LifecycleCommand::ForceSleep => {
            let outcome = core.force_sleep(member, core_scope).await?;
            let sleep = match &outcome.sleep {
                Some(SleepApplied::Global(state)) => {
                    format!("agent is now globally {}", state.as_str())
                }
                Some(SleepApplied::Chat(_, state)) => {
                    format!("chat membership is now {}", state.as_str())
                }
                None => "sleep state unchanged".to_string(),
            };
            Ok(format!(
                "Force sleep: {}; {sleep}. Completed work is not undone.",
                interruptions(&outcome.interruptions)
            ))
        }
        LifecycleCommand::StopAndCheckChat => {
            let outcome: RefreshOutcome = core.stop_and_check_chat(member, core_scope).await?;
            Ok(refresh_note(&outcome))
        }
    }
}

fn refresh_note(outcome: &RefreshOutcome) -> String {
    let mut parts = vec![interruptions(&outcome.interruptions)];
    for (chat, channel, unread) in &outcome.instructed {
        parts.push(format!(
            "refresh requested in chat {chat} (channel {channel}, {unread} messages past the last read point)"
        ));
    }
    if !outcome.no_session.is_empty() {
        parts.push(format!(
            "no live session in {} chat(s), so no instruction was delivered",
            outcome.no_session.len()
        ));
    }
    format!(
        "Stop and check chat: {}. Delivery is not proof the agent read the history.",
        parts.join("; ")
    )
}

fn interruptions(entries: &[(ChannelId, InterruptOutcome)]) -> String {
    if entries.is_empty() {
        return "no sessions to interrupt".to_string();
    }
    let confirmed = entries
        .iter()
        .filter(|(_, o)| *o == InterruptOutcome::Confirmed)
        .count();
    let idle = entries
        .iter()
        .filter(|(_, o)| *o == InterruptOutcome::NoActiveTurn)
        .count();
    let unsupported = entries
        .iter()
        .filter(|(_, o)| *o == InterruptOutcome::Unsupported)
        .count();
    let failed: Vec<_> = entries
        .iter()
        .filter_map(|(_, o)| match o {
            InterruptOutcome::Failed(reason) => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    let fallback: Vec<_> = entries
        .iter()
        .filter_map(|(_, o)| match o {
            InterruptOutcome::FallbackClosed { reason } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    let mut parts = Vec::new();
    if confirmed > 0 {
        parts.push(format!("{confirmed} interrupted"));
    }
    if idle > 0 {
        parts.push(format!("{idle} had no active turn"));
    }
    if unsupported > 0 {
        parts.push(format!("{unsupported} unsupported"));
    }
    if !failed.is_empty() {
        parts.push(format!("{} failed ({})", failed.len(), failed.join(", ")));
    }
    if !fallback.is_empty() {
        parts.push(format!(
            "{} session(s) closed as a fallback ({})",
            fallback.len(),
            fallback.join(", ")
        ));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chaos_core::lifecycle::ControlOutcome;

    fn outcome(entries: Vec<InterruptOutcome>) -> ControlOutcome {
        ControlOutcome {
            interruptions: entries
                .into_iter()
                .enumerate()
                .map(|(i, o)| (ChannelId(i as i64 + 1), o))
                .collect(),
            sleep: None,
        }
    }

    #[test]
    fn confirmed_cancellation_does_not_claim_rollback() {
        let note = format!(
            "Stop: {}. Completed work is not undone.",
            interruptions(&outcome(vec![InterruptOutcome::Confirmed]).interruptions)
        );
        assert_eq!(note, "Stop: 1 interrupted. Completed work is not undone.");
        assert!(!note.contains("undone") || note.contains("not undone"));
    }

    #[test]
    fn unsupported_and_fallback_are_visible() {
        let text = interruptions(
            &outcome(vec![
                InterruptOutcome::Unsupported,
                InterruptOutcome::Failed("offline".into()),
                InterruptOutcome::FallbackClosed {
                    reason: "cancel failed".into(),
                },
            ])
            .interruptions,
        );
        assert!(text.contains("1 unsupported"));
        assert!(text.contains("failed (offline)"));
        assert!(text.contains("session(s) closed as a fallback (cancel failed)"));
    }

    #[test]
    fn refresh_note_shows_read_state_without_claiming_compliance() {
        let note = refresh_note(&RefreshOutcome {
            interruptions: vec![(ChannelId(2), InterruptOutcome::Confirmed)],
            instructed: vec![(ChatId(3), ChannelId(2), 7)],
            no_session: Vec::new(),
        });
        assert!(note.contains("1 interrupted"));
        assert!(note.contains("7 messages past the last read point"));
        assert!(note.contains("not proof"));
    }
}
