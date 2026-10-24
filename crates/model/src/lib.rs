//! Plain data for Chaos Control: projects, channels, agents, mentions and messages.
//!
//! This crate has no UI dependencies so it stays cheap to compile and easy to test.
//! Strings are `Arc<str>` so the UI can turn them into `SharedString`s without copying.

mod sample;
pub mod search;

pub use sample::{demo_history, sample};
pub use search::{
    CorpusChannel, SearchAction, SearchCorpus, SearchHit, SearchProvider, SearchResult,
    SubstringSearch,
};

use std::sync::Arc;

/// Stable identifier for a channel, agent or message. Cheap to copy and hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub u32);

#[derive(Clone, Debug)]
pub struct Project {
    pub id: Id,
    pub name: Arc<str>,
    pub emoji: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub id: Id,
    pub name: Arc<str>,
    /// Optional link to a Linear issue. Issues never become channels on their own.
    pub linear_issue: Option<Arc<str>>,
    pub unread: bool,
    /// The project-wide room every project has.
    pub project_wide: bool,
}

/// Whether an agent can be talked to right now. Drawn as a coloured glow under
/// its pill in Agent Status (colours come from the theme, see
/// `ui::Theme::agent_state`):
///
/// - green, [`AgentState::Active`]: running and ready to read and reply.
/// - purple, [`AgentState::Sleeping`]: idle; it needs a ping before it picks
///   anything up.
/// - red, [`AgentState::Unresponsive`]: missing or not answering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Active,
    Sleeping,
    Unresponsive,
}

impl AgentState {
    pub fn label(self) -> &'static str {
        match self {
            AgentState::Active => "Active",
            AgentState::Sleeping => "Sleeping, needs a ping",
            AgentState::Unresponsive => "Unresponsive",
        }
    }
}

/// Which bundled brand mark to draw for an agent or a quick link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Brand {
    GitHub,
    Linear,
    Zed,
    Vercel,
    OpenAi,
    Claude,
    Devin,
    Copilot,
    Mistral,
    Opencode,
    Pi,
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub id: Id,
    pub name: Arc<str>,
    pub brand: Brand,
    pub state: AgentState,
}

#[derive(Clone, Debug)]
pub struct QuickLink {
    pub label: Arc<str>,
    pub brand: Brand,
    pub url: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct Mention {
    pub id: Id,
    pub agent: Id,
    pub channel: Id,
    pub text: Arc<str>,
}

/// Who wrote a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author {
    Agent(Id),
    Human,
}

/// One complete message. Agents post a message once their response is done;
/// nothing is streamed in token by token.
#[derive(Clone, Debug)]
pub struct Message {
    pub id: Id,
    pub author: Author,
    /// Seconds since the Unix epoch.
    pub sent_at: u64,
    pub body: Arc<str>,
}

/// Everything the static shell shows. Will be replaced by real sources later.
#[derive(Clone, Debug)]
pub struct Workspace {
    pub project: Project,
    pub channels: Vec<Channel>,
    pub quick_links: Vec<QuickLink>,
    pub agents: Vec<Agent>,
    pub mentions: Vec<Mention>,
}

impl Workspace {
    pub fn channel(&self, id: Id) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn agent(&self, id: Id) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }
}

/// Append-only message log for one channel.
///
/// Messages arrive whole and are only ever appended, so the UI can splice the new
/// rows onto the end of its virtualized list instead of re-measuring history.
///
/// The messages sit behind an `Arc` so search can take a snapshot without
/// copying; appending while a snapshot is alive copies once (copy-on-write).
#[derive(Debug, Default)]
pub struct ChannelLog {
    messages: Arc<Vec<Message>>,
}

impl ChannelLog {
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn get(&self, ix: usize) -> Option<&Message> {
        self.messages.get(ix)
    }

    /// Appends a batch and returns the index range the new messages occupy.
    pub fn extend(&mut self, batch: impl IntoIterator<Item = Message>) -> std::ops::Range<usize> {
        let messages = Arc::make_mut(&mut self.messages);
        let start = messages.len();
        messages.extend(batch);
        start..messages.len()
    }

    /// The whole history as it is now, for reading off the UI thread.
    pub fn snapshot(&self) -> Arc<Vec<Message>> {
        self.messages.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_matches_the_wireframe() {
        let ws = sample();
        assert_eq!(ws.project.name.as_ref(), "Starpoint");
        assert_eq!(ws.channels.len(), 3);
        assert!(ws.channels[0].project_wide);
        assert_eq!(ws.agents.len(), 8, "Agent Status is a 4x2 grid");
        assert_eq!(ws.quick_links.len(), 4);
        for m in &ws.mentions {
            assert!(
                ws.agent(m.agent).is_some(),
                "mention points at a known agent"
            );
            assert!(
                ws.channel(m.channel).is_some(),
                "mention points at a known channel"
            );
        }
    }

    #[test]
    fn log_appends_return_new_range() {
        let mut log = ChannelLog::default();
        let r = log.extend(demo_history(3));
        assert_eq!(r, 0..3);
        let r = log.extend(demo_history(2));
        assert_eq!(r, 3..5);
        assert_eq!(log.len(), 5);
    }

    #[test]
    fn snapshots_do_not_see_later_appends() {
        let mut log = ChannelLog::default();
        log.extend(demo_history(3));
        let snapshot = log.snapshot();
        log.extend(demo_history(2));
        assert_eq!(snapshot.len(), 3);
        assert_eq!(log.len(), 5);
    }

    #[test]
    fn demo_history_is_deterministic() {
        let a: Vec<_> = demo_history(50).map(|m| m.body).collect();
        let b: Vec<_> = demo_history(50).map(|m| m.body).collect();
        assert_eq!(a, b);
    }
}
