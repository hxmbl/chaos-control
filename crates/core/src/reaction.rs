//! Emoji reactions: what counts as one, and how they are grouped.
//!
//! Reactions are the room's quietest signal. They are stored and shown, and a
//! pinged agent's reaction answers its reply obligation, but they never wake
//! or notify anyone: not the message's author, not anyone mentioned in it.
//! The rules are in `docs/agents.md` ("Reactions").

use std::{collections::HashMap, sync::Arc};

use crate::types::{MemberId, Reaction};

/// Longest emoji accepted, in characters (a family or flag sequence with
/// joiners and modifiers fits; a sentence doesn't). The store's CHECK uses the
/// same number.
pub const MAX_EMOJI_CHARS: usize = 32;

/// Reactions that do NOT answer a reply obligation on their own: 👀 means
/// "on it", so the agent still owes the answer.
pub const HOLDS_OBLIGATION: &[&str] = &["👀"];

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

/// Whether a reaction with this (normalised) emoji answers a reply obligation.
pub fn answers_obligation(emoji: &str) -> bool {
    !HOLDS_OBLIGATION.contains(&emoji)
}

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
    fn only_eyes_holds_the_obligation() {
        assert!(answers_obligation("✅"));
        assert!(answers_obligation("👍"));
        assert!(answers_obligation("💀"));
        assert!(!answers_obligation("👀"));
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
