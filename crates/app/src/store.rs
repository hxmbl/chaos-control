//! The app's single source of truth for chat state.
//!
//! Views hold an `Entity<ChatStore>` and subscribe to its events instead of
//! sharing data by cloning it around. Each event says exactly what changed, so a
//! view only re-renders (or re-measures list rows) for the part it shows.

use std::{collections::HashMap, sync::Arc};

use gpui::{Context, EventEmitter};
use model::{Author, ChannelLog, CorpusChannel, Id, Message, SearchCorpus, Workspace};

pub enum StoreEvent {
    /// The selected channel changed.
    Selected,
    /// Whole messages were appended to a channel's log at `range`.
    Appended {
        channel: Id,
        range: std::ops::Range<usize>,
    },
    /// Scroll to and highlight a message (from search). Follows `Selected` when
    /// the message is in another channel.
    Reveal { channel: Id, index: usize },
}

pub struct ChatStore {
    workspace: Workspace,
    logs: HashMap<Id, ChannelLog>,
    selected: Id,
    next_message_id: u32,
}

impl EventEmitter<StoreEvent> for ChatStore {}

impl ChatStore {
    pub fn new(workspace: Workspace) -> Self {
        let selected = workspace.channels[0].id;
        let logs = workspace
            .channels
            .iter()
            .map(|c| (c.id, ChannelLog::default()))
            .collect();
        Self {
            workspace,
            logs,
            selected,
            next_message_id: 1_000_000,
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn selected(&self) -> Id {
        self.selected
    }

    pub fn selected_channel(&self) -> &model::Channel {
        self.workspace
            .channel(self.selected)
            .unwrap_or(&self.workspace.channels[0])
    }

    pub fn log(&self, channel: Id) -> Option<&ChannelLog> {
        self.logs.get(&channel)
    }

    pub fn select(&mut self, channel: Id, cx: &mut Context<Self>) {
        if self.selected != channel && self.workspace.channel(channel).is_some() {
            self.selected = channel;
            if let Some(c) = self.workspace.channels.iter_mut().find(|c| c.id == channel) {
                c.unread = false;
            }
            cx.emit(StoreEvent::Selected);
            cx.notify();
        }
    }

    /// Switches to `channel` and asks its view to scroll to the message at
    /// `index` in its log (logs are append-only, so indices are stable).
    pub fn reveal(&mut self, channel: Id, index: usize, cx: &mut Context<Self>) {
        self.select(channel, cx);
        if self.selected == channel {
            cx.emit(StoreEvent::Reveal { channel, index });
        }
    }

    /// Everything search reads, as a snapshot it can take to another thread.
    /// Only clones `Arc`s, so it is cheap even with very large histories.
    pub fn search_corpus(&self) -> SearchCorpus {
        SearchCorpus {
            channels: self
                .workspace
                .channels
                .iter()
                .map(|c| CorpusChannel {
                    id: c.id,
                    name: c.name.clone(),
                    messages: self
                        .logs
                        .get(&c.id)
                        .map(|log| log.snapshot())
                        .unwrap_or_default(),
                })
                .collect(),
        }
    }

    /// Appends complete messages to a channel. This is where messages from agents
    /// will land once there is a backend: one call per finished message (or batch).
    pub fn append(&mut self, channel: Id, batch: Vec<Message>, cx: &mut Context<Self>) {
        let Some(log) = self.logs.get_mut(&channel) else {
            return;
        };
        let range = log.extend(batch);
        if !range.is_empty() {
            cx.emit(StoreEvent::Appended { channel, range });
        }
    }

    /// Local echo for the composer until there is somewhere to send to.
    pub fn post_from_human(&mut self, body: &str, cx: &mut Context<Self>) {
        let body = body.trim();
        if body.is_empty() {
            return;
        }
        let id = Id(self.next_message_id);
        self.next_message_id += 1;
        let sent_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let message = Message {
            id,
            author: Author::Human,
            sent_at,
            body: Arc::from(body),
            reply_to: None,
        };
        self.append(self.selected, vec![message], cx);
    }

    pub fn load_demo_history(&mut self, count: usize, cx: &mut Context<Self>) {
        let main = self.workspace.channels[0].id;
        self.append(main, model::demo_history(count).collect(), cx);
    }
}
