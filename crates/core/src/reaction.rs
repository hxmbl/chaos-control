//! Emoji reactions: what counts as one, and how they are grouped.
//!
//! Reactions are the room's quietest signal. They are stored and shown, and a
//! pinged agent's reaction answers its reply obligation, but they never wake
//! or notify anyone: not the message's author, not anyone mentioned in it.
//! Which emoji answer a ping is configurable ([`PingAnswers`]): per agent in
//! `agents.json`, else per project in the store, else ✅ and 👍.
//! The rules are in `docs/agents.md` ("Reactions").

use std::{collections::HashMap, sync::Arc};

use crate::types::{MemberId, Reaction};

/// Longest emoji accepted, in characters (a family or flag sequence with
/// joiners and modifiers fits; a sentence doesn't). The store's CHECK uses the
/// same number.
pub const MAX_EMOJI_CHARS: usize = 32;

/// Reactions that never answer a reply obligation, whatever the config says:
/// 👀 means "on it", so the agent still owes the answer.
pub const HOLDS_OBLIGATION: &[&str] = &["👀"];

/// The emoji that answer a ping when neither the agent nor the project says
/// otherwise (STRM-402: ✅ and 👍; 💀 and the rest don't).
pub const DEFAULT_PING_ANSWERS: &[&str] = &["✅", "👍"];

/// In a ping-answer list, any emoji except 👀.
pub const ANY_EMOJI: &str = "*";

/// The project setting key in the store, and the `agents.json` field.
pub const PING_ANSWERS_KEY: &str = "ping_answer_reactions";

/// Slack-style shortcodes agents tend to send, mapped to the emoji itself so
/// `:+1:` and 👍 are the same reaction.
const SHORTCODES: &[(&str, &str)] = &[
    ("+1", "👍"),
    ("thumbsup", "👍"),
    ("thumbs_up", "👍"),
    ("-1", "👎"),
    ("thumbsdown", "👎"),
    ("white_check_mark", "✅"),
    ("heavy_check_mark", "✅"),
    ("check", "✅"),
    ("eyes", "👀"),
    ("skull", "💀"),
    ("heart", "❤️"),
    ("joy", "😂"),
    ("tada", "🎉"),
    ("fire", "🔥"),
    ("pray", "🙏"),
    ("rocket", "🚀"),
    ("thinking", "🤔"),
    ("thinking_face", "🤔"),
    ("x", "❌"),
];

/// The emoji as stored, or `None` if it isn't one: trimmed, `:shortcode:`
/// colons dropped, known shortcodes mapped to their emoji. Anything else
/// must be a short run with no spaces and no ASCII letters or digits (an
/// emoji, not a word).
pub fn normalize(raw: &str) -> Option<Arc<str>> {
    let s = raw.trim();
    let s = s
        .strip_prefix(':')
        .and_then(|s| s.strip_suffix(':'))
        .unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    if let Some((_, e)) = SHORTCODES.iter().find(|(code, _)| *code == lower) {
        return Some(Arc::from(*e));
    }
    let chars = s.chars().count();
    let wordy = s
        .chars()
        .any(|c| c.is_whitespace() || c.is_ascii_alphanumeric() || c.is_control());
    // Keycap sequences (1️⃣, #️⃣) start with an ASCII digit or #/*; allow
    // them when the keycap mark follows.
    let keycap = s.contains('\u{20E3}');
    if chars > MAX_EMOJI_CHARS || (wordy && !keycap) || s.is_ascii() {
        return None;
    }
    Some(Arc::from(s))
}

/// Which reactions answer a ping: close a pinged agent's reply obligation
/// as handled (`satisfied`, kind `react`). Whatever the list, 👀 never does,
/// and a reaction that doesn't answer is still recorded and still wakes
/// nobody; the obligation just stays open for the usual turn-end rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PingAnswers {
    /// `"*"`: any emoji except 👀.
    Any,
    /// Exactly these (normalised) emoji. Empty means no reaction answers.
    Only(Vec<Arc<str>>),
}

impl Default for PingAnswers {
    fn default() -> Self {
        PingAnswers::Only(DEFAULT_PING_ANSWERS.iter().map(|e| Arc::from(*e)).collect())
    }
}

impl PingAnswers {
    /// Parses a configured list: each entry is an emoji or a shortcode
    /// (`:white_check_mark:` is ✅, the same normalisation as reacting), or
    /// `"*"` for any emoji but 👀. Duplicates collapse. Every entry that is
    /// neither, and 👀 itself, is reported together, so one error names all
    /// the junk.
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> Result<PingAnswers, PingAnswersError> {
        let mut any = false;
        let mut out: Vec<Arc<str>> = Vec::new();
        let mut bad = Vec::new();
        for raw in entries {
            let raw = raw.as_ref();
            if raw.trim() == ANY_EMOJI {
                any = true;
                continue;
            }
            match normalize(raw) {
                Some(e) if HOLDS_OBLIGATION.contains(&&*e) => bad.push(BadEntry {
                    entry: raw.to_string(),
                    why: "👀 means \"on it\" and never answers a ping",
                }),
                Some(e) => {
                    if !out.contains(&e) {
                        out.push(e);
                    }
                }
                None => bad.push(BadEntry {
                    entry: raw.to_string(),
                    why: "not an emoji or a known :shortcode:",
                }),
            }
        }
        if !bad.is_empty() {
            return Err(PingAnswersError(bad));
        }
        Ok(if any {
            PingAnswers::Any
        } else {
            PingAnswers::Only(out)
        })
    }

    /// Whether a reaction with this (normalised) emoji answers a ping.
    pub fn answers(&self, emoji: &str) -> bool {
        if HOLDS_OBLIGATION.contains(&emoji) {
            return false;
        }
        match self {
            PingAnswers::Any => true,
            PingAnswers::Only(list) => list.iter().any(|e| &**e == emoji),
        }
    }

    /// The list as configured and stored: `["*"]` or the emoji.
    pub fn to_list(&self) -> Vec<String> {
        match self {
            PingAnswers::Any => vec![ANY_EMOJI.to_string()],
            PingAnswers::Only(list) => list.iter().map(|e| e.to_string()).collect(),
        }
    }

    /// For prompts: "✅ or 👍", "any emoji but 👀", "no emoji".
    pub fn describe(&self) -> String {
        match self {
            PingAnswers::Any => "any emoji but 👀".to_string(),
            PingAnswers::Only(list) if list.is_empty() => "no emoji".to_string(),
            PingAnswers::Only(list) => {
                let all: Vec<&str> = list.iter().map(|e| &**e).collect();
                match all.split_last() {
                    Some((last, [])) => last.to_string(),
                    Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
                    None => unreachable!("non-empty"),
                }
            }
        }
    }
}

/// One rejected entry of a ping-answer list and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadEntry {
    pub entry: String,
    pub why: &'static str,
}

/// A ping-answer list with junk in it: every bad entry, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PingAnswersError(pub Vec<BadEntry>);

impl std::fmt::Display for PingAnswersError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid ping_answer_reactions: ")?;
        for (i, b) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{:?} ({})", b.entry, b.why)?;
        }
        f.write_str(". Use emoji, :shortcodes: or \"*\" (any emoji but 👀)")
    }
}

impl std::error::Error for PingAnswersError {}

/// One emoji on one message and who put it there, in the order they reacted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub emoji: Arc<str>,
    pub members: Vec<MemberId>,
}

/// Groups reactions on one message by emoji, ordered by each emoji's first
/// use (Slack's order), members in the order they reacted.
pub fn group(reactions: &[Reaction]) -> Vec<Group> {
    let mut sorted: Vec<&Reaction> = reactions.iter().collect();
    sorted.sort_by_key(|r| (r.created_at, r.member));
    let mut out: Vec<Group> = Vec::new();
    let mut ix: HashMap<&str, usize> = HashMap::new();
    for r in sorted {
        match ix.get(&*r.emoji) {
            Some(&i) => out[i].members.push(r.member),
            None => {
                ix.insert(&r.emoji, out.len());
                out.push(Group {
                    emoji: r.emoji.clone(),
                    members: vec![r.member],
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChannelId, MessageId};

    #[test]
    fn normalizes_emoji_and_shortcodes() {
        assert_eq!(normalize(" 👍 ").as_deref(), Some("👍"));
        assert_eq!(normalize(":+1:").as_deref(), Some("👍"));
        assert_eq!(normalize("thumbsup").as_deref(), Some("👍"));
        assert_eq!(normalize("white_check_mark").as_deref(), Some("✅"));
        assert_eq!(normalize(":Skull:").as_deref(), Some("💀"));
        assert_eq!(normalize("👨‍👩‍👧‍👦").as_deref(), Some("👨‍👩‍👧‍👦"));
        assert_eq!(normalize("1️⃣").as_deref(), Some("1️⃣"));
        assert_eq!(normalize("🇬🇧").as_deref(), Some("🇬🇧"));
    }

    #[test]
    fn rejects_words_and_blanks() {
        for bad in [
            "",
            "   ",
            "::",
            "lol",
            "nice one",
            "ok",
            ":not_a_code:",
            "👍 👍",
            "a👍",
        ] {
            assert_eq!(normalize(bad), None, "{bad:?}");
        }
        let long = "😀".repeat(MAX_EMOJI_CHARS + 1);
        assert_eq!(normalize(&long), None);
    }

    #[test]
    fn default_ping_answers_are_check_and_thumbs_up() {
        let d = PingAnswers::default();
        assert!(d.answers("✅"));
        assert!(d.answers("👍"));
        assert!(!d.answers("💀"));
        assert!(!d.answers("👀"));
        assert_eq!(d.describe(), "✅ or 👍");
    }

    #[test]
    fn wildcard_is_anything_but_eyes() {
        let any = PingAnswers::parse(&["*"]).unwrap();
        assert_eq!(any, PingAnswers::Any);
        assert!(any.answers("💀"));
        assert!(any.answers("🇬🇧"));
        assert!(!any.answers("👀"));
        assert_eq!(any.to_list(), vec!["*"]);
        // "*" beside emoji is still the wildcard.
        assert_eq!(
            PingAnswers::parse(&["✅", " * "]).unwrap(),
            PingAnswers::Any
        );
    }

    #[test]
    fn ping_answers_normalise_shortcodes_and_collapse_duplicates() {
        let p = PingAnswers::parse(&[":white_check_mark:", "✅", "+1", "🚀"]).unwrap();
        assert_eq!(p.to_list(), vec!["✅", "👍", "🚀"]);
        assert_eq!(p.describe(), "✅, 👍 or 🚀");
        let none = PingAnswers::parse::<&str>(&[]).unwrap();
        assert!(!none.answers("✅"));
        assert_eq!(none.describe(), "no emoji");
    }

    #[test]
    fn ping_answers_reject_junk_and_eyes_together() {
        let err = PingAnswers::parse(&["✅", "lol", ":eyes:", "", "👍"]).unwrap_err();
        let entries: Vec<&str> = err.0.iter().map(|b| b.entry.as_str()).collect();
        assert_eq!(entries, vec!["lol", ":eyes:", ""]);
        let msg = err.to_string();
        assert!(msg.contains("\"lol\""), "{msg}");
        assert!(msg.contains("never answers a ping"), "{msg}");
    }

    #[test]
    fn groups_by_first_use() {
        let r = |member, emoji: &str, at| Reaction {
            message: MessageId(1),
            channel: ChannelId(1),
            member: MemberId(member),
            emoji: Arc::from(emoji),
            created_at: at,
        };
        let g = group(&[
            r(2, "💀", 20),
            r(1, "👍", 10),
            r(3, "👍", 30),
            r(3, "💀", 25),
        ]);
        assert_eq!(g.len(), 2);
        assert_eq!(&*g[0].emoji, "👍");
        assert_eq!(g[0].members, vec![MemberId(1), MemberId(3)]);
        assert_eq!(&*g[1].emoji, "💀");
        assert_eq!(g[1].members, vec![MemberId(2), MemberId(3)]);
    }
}
