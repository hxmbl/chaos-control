//! The permission log the right sidebar shows: every permission request an
//! agent made in the room, newest first, with how it ended.
//!
//! Requests no longer open the pane (they are muted); this log is where they
//! show instead. Reads are allowed at once ("Auto-allowed"); everything else
//! an agent on this computer asks for is declined at once ("Declined"), and
//! the agent carries on. Core keeps the requests (`permission_requests`);
//! this is the app's view of them.

use std::sync::Arc;

use crate::{Brand, Id, PermissionPrompt};

/// How many rows the app keeps in memory and asks core for.
pub const LOG_LEN: usize = 200;

/// How a request ended, as the log shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// Allowed without asking anyone: a read, or an agent in its own VM.
    AutoAllowed,
    /// Declined at once (never answered), or denied by a human.
    Declined,
    /// A human approved it (only possible through core for now).
    Approved,
}

impl PermissionOutcome {
    pub fn label(self) -> &'static str {
        match self {
            PermissionOutcome::AutoAllowed => "Auto-allowed",
            PermissionOutcome::Declined => "Declined",
            PermissionOutcome::Approved => "Approved",
        }
    }
}

/// One row of the permission log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionLogEntry {
    /// Core's request id (`#N`). Unique; newer requests have larger ids.
    pub request: i64,
    /// The agent's app id, when it is still a member.
    pub agent: Option<Id>,
    /// Its harness's mark, when known.
    pub brand: Option<Brand>,
    /// Its display name ("Previous agent" once it's gone).
    pub agent_name: Arc<str>,
    /// What it wanted to do: the tool call's title.
    pub action: Arc<str>,
    pub outcome: PermissionOutcome,
    /// When it asked, seconds since the Unix epoch.
    pub at: u64,
    /// A read-only tool call (ACP kind `read` or `search`).
    pub read_only: bool,
    /// Why, in its words (chat tool requests only).
    pub reason: Option<Arc<str>>,
    /// The tool's raw input, when the protocol gave one.
    pub detail: Option<Arc<str>>,
    /// Who it was routed to ("Hx"); empty for reads and VM agents.
    pub asked: Arc<str>,
}

impl PermissionLogEntry {
    /// The read-only pane view of this row (no Approve / Deny).
    pub fn prompt(&self) -> PermissionPrompt {
        PermissionPrompt {
            request: self.request,
            agent: self.agent_name.clone(),
            action: self.action.clone(),
            reason: self.reason.clone(),
            detail: self.detail.clone(),
            asked: self.asked.clone(),
            logged: Some(self.outcome),
        }
    }
}

/// The log, newest first, at most [`LOG_LEN`] rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionLog {
    entries: Vec<PermissionLogEntry>,
}

impl PermissionLog {
    pub fn entries(&self) -> &[PermissionLogEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Replaces the whole log (a project opened, core started).
    pub fn reset(&mut self, mut entries: Vec<PermissionLogEntry>) {
        entries.sort_by_key(|e| std::cmp::Reverse(e.request));
        entries.dedup_by_key(|e| e.request);
        entries.truncate(LOG_LEN);
        self.entries = entries;
    }

    /// Adds a new request, or updates one already listed in place (an
    /// answer changed its outcome). Keeps newest first and the cap.
    pub fn upsert(&mut self, entry: PermissionLogEntry) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.request == entry.request) {
            *e = entry;
            return;
        }
        let at = self
            .entries
            .iter()
            .position(|e| e.request < entry.request)
            .unwrap_or(self.entries.len());
        self.entries.insert(at, entry);
        self.entries.truncate(LOG_LEN);
    }

    pub fn get(&self, request: i64) -> Option<&PermissionLogEntry> {
        self.entries.iter().find(|e| e.request == request)
    }
}

/// A short relative time for a log row: "now", "4m", "2h", "3d", "5w".
pub fn relative_time(at: u64, now: u64) -> String {
    let s = now.saturating_sub(at);
    match s {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", s / 60),
        3_600..86_400 => format!("{}h", s / 3_600),
        86_400..604_800 => format!("{}d", s / 86_400),
        _ => format!("{}w", s / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(request: i64, outcome: PermissionOutcome) -> PermissionLogEntry {
        PermissionLogEntry {
            request,
            agent: Some(Id(1)),
            brand: Some(Brand::Claude),
            agent_name: Arc::from("Claude Code"),
            action: Arc::from(format!("tool {request}")),
            outcome,
            at: request as u64,
            read_only: outcome == PermissionOutcome::AutoAllowed,
            reason: None,
            detail: None,
            asked: Arc::from(""),
        }
    }

    fn ids(log: &PermissionLog) -> Vec<i64> {
        log.entries().iter().map(|e| e.request).collect()
    }

    #[test]
    fn upsert_keeps_newest_first_and_updates_in_place() {
        let mut log = PermissionLog::default();
        log.upsert(entry(2, PermissionOutcome::Declined));
        log.upsert(entry(5, PermissionOutcome::AutoAllowed));
        log.upsert(entry(3, PermissionOutcome::Declined));
        assert_eq!(ids(&log), [5, 3, 2]);
        log.upsert(entry(3, PermissionOutcome::Approved));
        assert_eq!(ids(&log), [5, 3, 2]);
        assert_eq!(log.get(3).unwrap().outcome, PermissionOutcome::Approved);
    }

    #[test]
    fn the_log_is_capped_from_the_old_end() {
        let mut log = PermissionLog::default();
        log.reset(
            (0..LOG_LEN as i64 + 5)
                .map(|i| entry(i, PermissionOutcome::Declined))
                .collect(),
        );
        assert_eq!(log.len(), LOG_LEN);
        assert_eq!(log.entries()[0].request, LOG_LEN as i64 + 4);
        log.upsert(entry(10_000, PermissionOutcome::AutoAllowed));
        assert_eq!(log.len(), LOG_LEN);
        assert_eq!(log.entries()[0].request, 10_000);
        // Older than everything kept: inserted at the end, then cut.
        log.upsert(entry(-1, PermissionOutcome::Declined));
        assert!(log.get(-1).is_none());
    }

    #[test]
    fn outcome_labels_and_relative_times() {
        assert_eq!(PermissionOutcome::AutoAllowed.label(), "Auto-allowed");
        assert_eq!(PermissionOutcome::Declined.label(), "Declined");
        assert_eq!(relative_time(100, 100), "now");
        assert_eq!(relative_time(100, 159), "now");
        assert_eq!(relative_time(0, 60 * 4), "4m");
        assert_eq!(relative_time(0, 3_600 * 2 + 5), "2h");
        assert_eq!(relative_time(0, 86_400 * 3), "3d");
        assert_eq!(relative_time(0, 604_800 * 5), "5w");
        // A clock that went backwards reads as now.
        assert_eq!(relative_time(500, 100), "now");
    }

    #[test]
    fn a_logged_entry_opens_read_only() {
        let p = entry(7, PermissionOutcome::Declined).prompt();
        assert_eq!(p.logged, Some(PermissionOutcome::Declined));
        let pane = crate::PaneContent::Permission(p);
        assert!(pane.actions().is_empty());
        assert_eq!(&*pane.title(&crate::sample()), "Claude Code · Declined");
    }
}
