//! Search over a workspace's channels and messages.
//!
//! The UI only talks to [`SearchProvider`], handing it an immutable
//! [`SearchCorpus`] snapshot on a background thread. Today the provider is
//! [`SubstringSearch`], a plain case-insensitive scan; ranking, semantic search
//! or a command palette can replace it later without touching the UI.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{Author, Id, Message};

/// What can be searched, frozen at one moment. Building one only clones `Arc`s,
/// so the UI can hand it to a background thread and keep appending messages.
#[derive(Clone, Debug, Default)]
pub struct SearchCorpus {
    /// Channels in sidebar order.
    pub channels: Vec<CorpusChannel>,
}

#[derive(Clone, Debug)]
pub struct CorpusChannel {
    pub id: Id,
    pub name: Arc<str>,
    /// The channel's whole history, oldest first (see `ChannelLog::snapshot`).
    pub messages: Arc<Vec<Message>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchHit {
    /// A channel whose name matches.
    Channel { channel: Id },
    /// A message whose text matches. `index` is its position in the channel log.
    Message {
        channel: Id,
        index: usize,
        message: Id,
        author: Author,
        sent_at: u64,
        /// One line of the body around the first match.
        snippet: Arc<str>,
    },
}

/// Something the user can do with a result ("New chat", "Link this in main").
/// Reserved for the command-palette side of search; nothing produces these yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchAction {
    /// Stable identifier the UI dispatches on.
    pub id: Arc<str>,
    pub label: Arc<str>,
}

/// One entry in the results: what matched, plus anything that can be done
/// with it. The UI shows the selected result as a card; once actions exist they
/// are listed under it as rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub hit: SearchHit,
    /// Empty for plain matches (everything `SubstringSearch` returns).
    pub actions: Vec<SearchAction>,
}

impl From<SearchHit> for SearchResult {
    fn from(hit: SearchHit) -> Self {
        Self {
            hit,
            actions: Vec::new(),
        }
    }
}

/// Anything that can answer a search. Called off the UI thread.
pub trait SearchProvider: Send + Sync {
    /// Up to `limit` hits for `query`, best first. Long scans should check
    /// `cancel` now and then and return early once it is set: a newer query has
    /// replaced this one and the result will be thrown away.
    fn search(
        &self,
        corpus: &SearchCorpus,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Vec<SearchResult>;
}

/// Case-insensitive substring match: channel names first (sidebar order), then
/// messages, newest first across all channels.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubstringSearch;

/// How many messages to scan between cancellation checks.
const CANCEL_CHECK_EVERY: usize = 4096;

impl SearchProvider for SubstringSearch {
    fn search(
        &self,
        corpus: &SearchCorpus,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Vec<SearchResult> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let needle = Needle::new(query);

        let mut hits: Vec<SearchHit> = corpus
            .channels
            .iter()
            .filter(|c| needle.find(&c.name).is_some())
            .map(|c| SearchHit::Channel { channel: c.id })
            .take(limit)
            .collect();

        // Newest matches per channel (each channel stops at `limit`), then merged.
        let mut messages = Vec::new();
        for channel in &corpus.channels {
            let mut found = 0;
            for (scanned, (index, message)) in channel.messages.iter().enumerate().rev().enumerate()
            {
                if scanned % CANCEL_CHECK_EVERY == 0 && cancel.load(Ordering::Relaxed) {
                    return Vec::new();
                }
                if let Some(at) = needle.find(&message.body) {
                    messages.push(SearchHit::Message {
                        channel: channel.id,
                        index,
                        message: message.id,
                        author: message.author,
                        sent_at: message.sent_at,
                        snippet: snippet(&message.body, at),
                    });
                    found += 1;
                    if found == limit {
                        break;
                    }
                }
            }
        }
        messages.sort_by_key(|hit| match hit {
            SearchHit::Message { sent_at, .. } => std::cmp::Reverse(*sent_at),
            SearchHit::Channel { .. } => std::cmp::Reverse(0),
        });
        let room = limit - hits.len();
        hits.extend(messages.into_iter().take(room));
        hits.into_iter().map(SearchResult::from).collect()
    }
}

/// A lowercased query. ASCII queries (the common case) match without allocating.
struct Needle {
    lower: String,
    ascii: bool,
}

impl Needle {
    fn new(query: &str) -> Self {
        Self {
            lower: query.to_lowercase(),
            ascii: query.is_ascii(),
        }
    }

    /// Byte offset of the first case-insensitive match in `haystack`.
    fn find(&self, haystack: &str) -> Option<usize> {
        let needle = self.lower.as_bytes();
        if self.ascii {
            // Every byte of a multi-byte UTF-8 character is >= 0x80, so an ASCII
            // needle can only match at a character boundary.
            return haystack
                .as_bytes()
                .windows(needle.len())
                .position(|w| w.eq_ignore_ascii_case(needle));
        }
        let mut lowered = String::with_capacity(haystack.len());
        let mut origin = Vec::with_capacity(haystack.len());
        for (ix, ch) in haystack.char_indices() {
            for lower in ch.to_lowercase() {
                let start = lowered.len();
                lowered.push(lower);
                origin.extend(std::iter::repeat_n(ix, lowered.len() - start));
            }
        }
        lowered.find(&self.lower).map(|at| origin[at])
    }
}

/// Characters of context kept before the match.
const SNIPPET_LEAD: usize = 24;
/// Characters kept in total; the UI ellipsizes whatever does not fit.
const SNIPPET_LEN: usize = 140;

/// One line of `body` starting a little before byte offset `at`, cut at a word
/// boundary when one is near, with "…" when it does not start at the beginning.
fn snippet(body: &str, at: usize) -> Arc<str> {
    let before: Vec<(usize, char)> = body[..at].char_indices().collect();
    let mut start = before
        .len()
        .checked_sub(SNIPPET_LEAD)
        .map_or(0, |ix| before[ix].0);
    if start > 0
        && let Some(space) = body[start..at].find(char::is_whitespace)
    {
        start += space + 1;
    }
    let text: String = body[start..]
        .chars()
        .take(SNIPPET_LEN)
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    if start > 0 {
        format!("…{}", text.trim_start()).into()
    } else {
        text.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u32, sent_at: u64, body: &str) -> Message {
        Message {
            id: Id(id),
            author: Author::Human,
            sent_at,
            body: Arc::from(body),
            reply_to: None,
        }
    }

    fn corpus() -> SearchCorpus {
        SearchCorpus {
            channels: vec![
                CorpusChannel {
                    id: Id(1),
                    name: "main".into(),
                    messages: Arc::new(vec![
                        msg(10, 100, "Pushed the fix for the SYNC layer"),
                        msg(11, 300, "Nothing to see"),
                        msg(12, 500, "sync is green again"),
                    ]),
                },
                CorpusChannel {
                    id: Id(2),
                    name: "sync-layer".into(),
                    messages: Arc::new(vec![msg(20, 400, "Résumé: Sync déjà vu")]),
                },
            ],
        }
    }

    fn search(query: &str, limit: usize) -> Vec<SearchHit> {
        SubstringSearch
            .search(&corpus(), query, limit, &AtomicBool::new(false))
            .into_iter()
            .map(|r| {
                assert!(r.actions.is_empty(), "plain matches carry no actions");
                r.hit
            })
            .collect()
    }

    fn message_ids(hits: &[SearchHit]) -> Vec<u32> {
        hits.iter()
            .filter_map(|h| match h {
                SearchHit::Message { message, .. } => Some(message.0),
                SearchHit::Channel { .. } => None,
            })
            .collect()
    }

    #[test]
    fn channels_come_first_then_messages_newest_first() {
        let hits = search("SyNc", 20);
        assert_eq!(hits[0], SearchHit::Channel { channel: Id(2) });
        assert_eq!(message_ids(&hits), vec![12, 20, 10]);
    }

    #[test]
    fn message_hits_point_back_at_their_log_position() {
        let hits = search("green", 20);
        assert!(matches!(
            hits[..],
            [SearchHit::Message {
                channel: Id(1),
                index: 2,
                ..
            }]
        ));
    }

    #[test]
    fn limit_caps_the_total() {
        assert_eq!(search("sync", 2).len(), 2);
        assert!(search("", 20).is_empty());
        assert!(search("   ", 20).is_empty());
    }

    #[test]
    fn non_ascii_matches_case_insensitively() {
        assert_eq!(message_ids(&search("DÉJÀ", 20)), vec![20]);
        assert_eq!(message_ids(&search("résumé", 20)), vec![20]);
    }

    #[test]
    fn cancelled_search_returns_nothing() {
        let hits = SubstringSearch.search(&corpus(), "sync", 20, &AtomicBool::new(true));
        assert!(hits.is_empty());
    }

    #[test]
    fn snippet_keeps_context_and_respects_char_boundaries() {
        let body = "ééééé ".repeat(10) + "the needle is here";
        let at = body.find("needle").unwrap();
        let s = snippet(&body, at);
        assert!(s.starts_with('…'));
        assert!(s.contains("the needle is here"));
        assert_eq!(snippet("needle first", 0).as_ref(), "needle first");
        assert_eq!(
            snippet("line one\nline two", 9).as_ref(),
            "line one line two"
        );
    }
}
