//! Unicode emoji registry and shortcode resolution.
//!
//! A single source of truth for emoji names/shortcodes, used by reactions,
//! composed messages, and agent APIs. Backed by the Unicode emoji set
//! (`emojis` crate); workspace-defined custom emoji are out of scope.

use std::borrow::Cow;
use std::sync::Arc;

/// A match returned by emoji search/autocomplete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmojiMatch {
    /// Canonical shortcode without colons, e.g. `thumbs_up`.
    pub shortcode: Arc<str>,
    /// The Unicode emoji value, e.g. `👍` or `👍🏽`.
    pub emoji: Arc<str>,
    /// Human-readable name, e.g. "thumbs up".
    pub name: Arc<str>,
}

/// Known aliases that are accepted as shortcodes even when not the canonical
/// shortcode exposed by the emoji dataset.
const ALIASES: &[(&str, &str)] = &[
    ("+1", "+1"),
    ("-1", "-1"),
    ("thumbsup", "+1"),
    ("thumbs_up", "+1"),
    ("thumbsdown", "-1"),
    ("thumbs_down", "-1"),
    ("white_check_mark", "white_check_mark"),
    ("heavy_check_mark", "check_mark"),
    ("check", "white_check_mark"),
    ("thinking", "thinking_face"),
];

fn lookup_alias(name: &str) -> Option<&'static str> {
    ALIASES
        .iter()
        .find_map(|(a, target)| (*a == name).then_some(*target))
}

fn normalize_sep(s: &str) -> String {
    s.replace('-', "_")
}

/// Returns the canonical Unicode emoji for a shortcode name (without colons),
/// if known. Matching is case-insensitive and accepts common shortcode variants
/// (underscores/hyphens, Discord-style names, and known aliases).
pub fn resolve_shortcode(name: &str) -> Option<&'static emojis::Emoji> {
    let lower = name.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    for c in shortcode_candidates(&lower) {
        if let Some(e) = emojis::get_by_shortcode(&c) {
            return Some(e);
        }
    }
    let wanted = shortcode_candidates(&lower);
    let wanted_norm: Vec<String> = wanted.iter().map(|w| normalize_sep(w)).collect();
    emojis::iter().find(|e| {
        e.shortcode().is_some_and(|sc| {
            let sc_l = sc.to_ascii_lowercase();
            wanted.iter().any(|w| w == &sc_l)
                || wanted_norm.iter().any(|w| w == &normalize_sep(&sc_l))
        }) || {
            let name_l = normalize_sep(&e.name().to_ascii_lowercase().replace(' ', "_"));
            wanted_norm.iter().any(|w| w == &name_l)
        }
    })
}

fn shortcode_candidates(lower: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |s: String| {
        if !s.is_empty() && !out.iter().any(|x| x == &s) {
            out.push(s);
        }
    };
    let norm_us = lower.replace('-', "_");
    let norm_hy = lower.replace('_', "-");

    push(lower.to_string());
    push(norm_us.clone());
    push(norm_hy.clone());

    for base in [lower, norm_us.as_str(), norm_hy.as_str()] {
        if let Some(target) = lookup_alias(base) {
            push(target.to_string());
            push(target.replace('-', "_"));
            push(target.replace('_', "-"));
        }
    }

    // Common Discord/alias fallbacks
    if norm_us == "thumbs_up" || norm_us == "thumbsup" {
        push("+1".to_string());
    }
    if norm_us == "thumbs_down" || norm_us == "thumbsdown" {
        push("-1".to_string());
    }
    out
}

/// True if `s` is exactly one Unicode emoji sequence (any skin tone/ZWJ/flag
/// variant supported by the Unicode emoji set).
pub fn is_unicode_emoji(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    emojis::get(s).is_some_and(|e| e.as_str() == s)
}

/// Parses a raw reaction or emoji input into its canonical emoji value.
///
/// Accepts a literal Unicode emoji or a completed shortcode (`:thumbs_up:` or
/// `thumbs_up` form as passed here). Returns `None` for words, blanks,
/// unknown shortcodes, or anything that is not a single emoji.
pub fn normalize_emoji_input(raw: &str) -> Option<Arc<str>> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let inner = s
        .strip_prefix(':')
        .and_then(|rest| rest.strip_suffix(':'))
        .unwrap_or(s);
    if inner.is_empty() {
        return None;
    }
    let lower = inner.to_ascii_lowercase();
    if let Some(e) = resolve_shortcode(&lower) {
        return Some(Arc::from(e.as_str()));
    }
    // Allow bare shortcode-like names only when they resolved above; otherwise
    // only a literal emoji (not wrapped in colons) is accepted.
    if s.starts_with(':') && s.ends_with(':') {
        return None;
    }
    if is_unicode_emoji(s) {
        return Some(Arc::from(s));
    }
    None
}

/// Resolves completed known `:shortcode:` tokens in `text` to their Unicode
/// emoji values. Unknown shortcodes and ordinary colon punctuation are left
/// unchanged.
pub fn resolve_shortcodes_in_text(text: &str) -> Cow<'_, str> {
    if !text.contains(':') {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut out: Option<String> = None;
    let mut last = 0;
    while i < bytes.len() {
        if bytes[i] != b':' {
            i += 1;
            continue;
        }
        // Don't trigger inside likely URLs like http:// or word:adjacent cases
        // are still fine if followed by shortcode chars and closing colon; we
        // only replace `:name:` tokens where name chars are shortcode-like.
        let start = i;
        i += 1;
        let name_start = i;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b':' {
                break;
            }
            if !(b.is_ascii_alphanumeric() || b == b'_' || b == b'+' || b == b'-') {
                i = name_start; // not a shortcode token; continue scanning after first ':'
                break;
            }
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b':' || i == name_start {
            // no closing colon or empty; advance past the opening colon
            i = start + 1;
            continue;
        }
        let name = &text[name_start..i];
        let end = i + 1;
        if let Some(emoji) = resolve_shortcode(name) {
            let buf = out.get_or_insert_with(|| String::with_capacity(text.len()));
            buf.push_str(&text[last..start]);
            buf.push_str(emoji.as_str());
            last = end;
        }
        i = end;
    }
    match out {
        Some(mut buf) => {
            buf.push_str(&text[last..]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(text),
    }
}

fn canonical_shortcode(e: &'static emojis::Emoji) -> &'static str {
    e.shortcode().unwrap_or_else(|| e.name())
}

/// Searches the emoji set by shortcode/name, returning up to `limit` matches.
/// Results prefer prefix matches on shortcode, then other substring matches.
/// Accepts separator variants (e.g. Discord-style `thumbs-up` matches
/// `thumbs_up`).
pub fn search(query: &str, limit: usize) -> Vec<EmojiMatch> {
    let q = query.trim().trim_start_matches(':').to_ascii_lowercase();
    if q.is_empty() || limit == 0 {
        return Vec::new();
    }
    let q_norm = q.replace('-', "_");
    let q_name_norm = q_norm.replace('_', " ");
    let exact = resolve_shortcode(&q);
    let mut prefix = Vec::new();
    let mut other = Vec::new();
    for e in emojis::iter() {
        let Some(sc) = e.shortcode() else { continue };
        let sc_l = sc.to_ascii_lowercase();
        let sc_norm = sc_l.replace('-', "_");
        let name_l = e.name().to_ascii_lowercase();
        let name_norm = name_l.replace('-', " ").replace('_', " ");
        let direct_hit = exact.is_some_and(|x| x.as_str() == e.as_str());
        let score = if direct_hit || sc_l.starts_with(&q) || sc_norm.starts_with(&q_norm) {
            Some((0usize, sc_l.len()))
        } else if sc_l.contains(&q)
            || sc_norm.contains(&q_norm)
            || sc_l.replace('_', "-").contains(&q)
        {
            Some((1, sc_l.len()))
        } else if name_l.contains(&q)
            || name_norm.contains(&q_name_norm)
            || name_l.replace(' ', "_").replace('-', "_").contains(&q_norm)
        {
            Some((2, sc_l.len()))
        } else {
            None
        };
        let Some(rank) = score else { continue };
        let m = EmojiMatch {
            shortcode: Arc::from(sc),
            emoji: Arc::from(e.as_str()),
            name: Arc::from(e.name()),
        };
        if rank.0 == 0 {
            prefix.push((rank, m));
        } else {
            other.push((rank, m));
        }
    }
    // If the query resolves to a known emoji but wasn't included (e.g. no
    // shortcode field), ensure it's present.
    if let Some(e) = exact {
        if !prefix.iter().any(|(_, m)| &*m.emoji == e.as_str())
            && !other.iter().any(|(_, m)| &*m.emoji == e.as_str())
        {
            let sc = canonical_shortcode(e);
            prefix.insert(
                0,
                (
                    (0, sc.len()),
                    EmojiMatch {
                        shortcode: Arc::from(sc),
                        emoji: Arc::from(e.as_str()),
                        name: Arc::from(e.name()),
                    },
                ),
            );
        }
    }
    prefix.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.shortcode.cmp(&b.1.shortcode)));
    other.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.shortcode.cmp(&b.1.shortcode)));
    prefix.extend(other);
    // de-dup by emoji value preserving order
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (_, m) in prefix {
        if seen.insert(m.emoji.as_ref().to_string()) {
            out.push(m);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// Returns up to `limit` emoji suggestions for an empty query (common emoji).
pub fn common(limit: usize) -> Vec<EmojiMatch> {
    if limit == 0 {
        return Vec::new();
    }
    const COMMON: &[&str] = &[
        "thumbs_up",
        "thumbs_down",
        "white_check_mark",
        "eyes",
        "heart",
        "joy",
        "tada",
        "fire",
        "pray",
        "rocket",
        "thinking_face",
        "x",
        "smile",
        "skull",
    ];
    let mut out = Vec::new();
    for sc in COMMON {
        if out.len() >= limit {
            break;
        }
        if let Some(e) = resolve_shortcode(sc) {
            out.push(EmojiMatch {
                shortcode: Arc::from(canonical_shortcode(e)),
                emoji: Arc::from(e.as_str()),
                name: Arc::from(e.name()),
            });
        }
    }
    if out.len() < limit {
        for e in emojis::iter().take(limit.saturating_mul(2)) {
            if out.len() >= limit {
                break;
            }
            let Some(sc) = e.shortcode() else { continue };
            if out.iter().any(|m| &*m.emoji == e.as_str()) {
                continue;
            }
            out.push(EmojiMatch {
                shortcode: Arc::from(sc),
                emoji: Arc::from(e.as_str()),
                name: Arc::from(e.name()),
            });
        }
    }
    out.truncate(limit);
    out
}

/// Suggest emoji for autocomplete: `common` when query is empty, else `search`.
pub fn suggest(query: &str, limit: usize) -> Vec<EmojiMatch> {
    let q = query.trim().trim_start_matches(':').to_ascii_lowercase();
    if q.is_empty() {
        common(limit)
    } else {
        search(&q, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_common_shortcodes_and_aliases() {
        assert_eq!(resolve_shortcode("thumbs_up").unwrap().as_str(), "👍");
        assert_eq!(resolve_shortcode("thumbsup").unwrap().as_str(), "👍");
        assert_eq!(resolve_shortcode("thumbs-up").unwrap().as_str(), "👍");
        assert_eq!(resolve_shortcode("+1").unwrap().as_str(), "👍");
        assert_eq!(
            resolve_shortcode("white_check_mark").unwrap().as_str(),
            "✅"
        );
        assert_eq!(
            resolve_shortcode("white-check-mark").unwrap().as_str(),
            "✅"
        );
        assert_eq!(resolve_shortcode("eyes").unwrap().as_str(), "👀");
    }

    #[test]
    fn normalizes_literal_and_shortcode_inputs() {
        assert_eq!(normalize_emoji_input("👍🏽").as_deref(), Some("👍🏽"));
        assert_eq!(normalize_emoji_input(":thumbs_up:").as_deref(), Some("👍"));
        assert_eq!(normalize_emoji_input(":thumbs-up:").as_deref(), Some("👍"));
        assert_eq!(normalize_emoji_input(":+1:").as_deref(), Some("👍"));
        assert_eq!(normalize_emoji_input("🇬🇧").as_deref(), Some("🇬🇧"));
        assert_eq!(normalize_emoji_input("👨‍👩‍👧‍👦").as_deref(), Some("👨‍👩‍👧‍👦"));
        assert_eq!(normalize_emoji_input(":not_a_real_shortcode:").as_deref(), None);
        assert_eq!(normalize_emoji_input("hello").as_deref(), None);
    }

    #[test]
    fn resolves_known_shortcodes_in_text_and_leaves_unknown() {
        assert_eq!(
            resolve_shortcodes_in_text("hi :thumbs_up: there"),
            "hi 👍 there"
        );
        assert_eq!(
            resolve_shortcodes_in_text("hi :thumbs-up: there"),
            "hi 👍 there"
        );
        assert_eq!(
            resolve_shortcodes_in_text("keep :not_a_real_shortcode: ok"),
            "keep :not_a_real_shortcode: ok"
        );
        assert_eq!(resolve_shortcodes_in_text("time 10:30"), "time 10:30");
    }

    #[test]
    fn search_finds_emoji() {
        let r = search("thumb", 10);
        assert!(r.iter().any(|m| &*m.emoji == "👍"));
        let r = search("thumbs-up", 10);
        assert!(r.iter().any(|m| &*m.emoji == "👍"));
    }
}
