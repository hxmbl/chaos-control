//! Highlighting known @mentions (rendered messages and composer input).
//!
//! Mentions are matched with the same token rules as `model::mentions`:
//! an `@` not preceded by a word character, followed by letters, digits,
//! `_`, `-` or `.`, with trailing dots treated as punctuation. Only known
//! project members (linked agents' handles and nicknames, matched
//! case-insensitively) and supported broadcast targets (`@everyone`,
//! `@here`, `@channel`) are highlighted; arbitrary `@text` is left alone.
//! Colors are deterministic for a member's stable identity and come from a
//! curated, theme-aware palette.

use std::{collections::HashMap, ops::Range, sync::LazyLock};

use gpui::{HighlightStyle, Hsla, rgb};
use ui::Appearance;

/// Broadcast mention targets supported by core.
const BROADCASTS: &[&str] = &["everyone", "here", "channel"];

/// Known mention tokens and the stable identity each one colors by.
/// Nicknames map to their member's handle so they share a color.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KnownMentions {
    /// Lowercased token -> canonical identity for coloring.
    by_token: HashMap<String, String>,
}

impl KnownMentions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a member: its handle and nicknames all color by `identity`
    /// (normally the handle). Matching is case-insensitive.
    pub fn insert_member(&mut self, identity: &str, handle: &str, nicknames: &[impl AsRef<str>]) {
        let identity = identity.to_ascii_lowercase();
        self.insert_token(handle, &identity);
        for nick in nicknames {
            self.insert_token(nick.as_ref(), &identity);
        }
    }

    /// Adds broadcast targets as known mentions.
    pub fn insert_broadcasts(&mut self) {
        for token in BROADCASTS {
            self.by_token
                .insert((*token).to_string(), (*token).to_string());
        }
    }

    fn insert_token(&mut self, token: &str, identity: &str) {
        let token = token.trim_start_matches('@').to_ascii_lowercase();
        if !token.is_empty() {
            self.by_token.insert(token, identity.to_string());
        }
    }

    /// The canonical coloring identity for `token` (without `@`), if known.
    pub fn identity_for(&self, token: &str) -> Option<&str> {
        self.by_token
            .get(&token.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }
}

/// A highlighted mention span in `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MentionHighlight {
    pub range: Range<usize>,
    pub identity: String,
}

/// Finds known mention spans in `text`, using the same token boundaries as
/// `model::mentions` (so it aligns with displayed text, including trailing
/// dots kept as punctuation).
pub fn find_mentions(text: &str, known: &KnownMentions) -> Vec<MentionHighlight> {
    if known.is_empty() || !text.contains('@') {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let starts = c == '@' && !prev.is_some_and(|p| p.is_alphanumeric() || p == '_');
        prev = Some(c);
        if !starts {
            continue;
        }
        let start = at + c.len_utf8();
        let mut end = start;
        while let Some(&(i, d)) = chars.peek() {
            if d.is_alphanumeric() || matches!(d, '_' | '-' | '.') {
                end = i + d.len_utf8();
                prev = Some(d);
                chars.next();
            } else {
                break;
            }
        }
        let raw = &text[start..end];
        let token = raw.trim_end_matches('.');
        if token.is_empty() {
            continue;
        }
        if let Some(identity) = known.identity_for(token) {
            // Highlight `@` + token, but not trailing punctuation dots.
            let span_end = start + token.len();
            out.push(MentionHighlight {
                range: at..span_end,
                identity: identity.to_string(),
            });
        }
    }
    out
}

/// Stable FNV-1a hash for deterministic color selection.
pub fn stable_hash(bytes: &str) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for b in bytes.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

static LIGHT_PALETTE: LazyLock<Vec<Hsla>> = LazyLock::new(|| {
    vec![
        c(0x0f766e), // teal
        c(0x1d4ed8), // blue
        c(0x7c3aed), // violet
        c(0xbe185d), // pink
        c(0xb45309), // amber
        c(0x15803d), // green
        c(0xc2410c), // orange
        c(0x0369a1), // sky
        c(0x6d28d9), // purple
        c(0xa16207), // yellow-brown
        c(0x0e7490), // cyan
        c(0xbe123c), // rose
    ]
});

static DARK_PALETTE: LazyLock<Vec<Hsla>> = LazyLock::new(|| {
    vec![
        c(0x5eead4), // teal
        c(0x93c5fd), // blue
        c(0xc4b5fd), // violet
        c(0xf9a8d4), // pink
        c(0xfcd34d), // amber
        c(0x86efac), // green
        c(0xfdba74), // orange
        c(0x7dd3fc), // sky
        c(0xd8b4fe), // purple
        c(0xfde68a), // yellow
        c(0x67e8f9), // cyan
        c(0xfda4af), // rose
    ]
});

/// A curated mention palette, tuned per appearance for adequate contrast.
fn palette(appearance: Appearance) -> &'static [Hsla] {
    match appearance {
        Appearance::Light => &LIGHT_PALETTE,
        Appearance::Dark => &DARK_PALETTE,
    }
}

/// Deterministic, theme-aware color for a mention identity.
pub fn mention_color(identity: &str, appearance: Appearance) -> Hsla {
    let colors = palette(appearance);
    let ix = (stable_hash(&identity.to_ascii_lowercase()) as usize) % colors.len();
    colors[ix]
}

/// Highlight styles for mention spans in `text`.
pub fn mention_highlights(
    text: &str,
    known: &KnownMentions,
    appearance: Appearance,
) -> Vec<(Range<usize>, HighlightStyle)> {
    find_mentions(text, known)
        .into_iter()
        .map(|m| {
            let color = mention_color(&m.identity, appearance);
            (
                m.range,
                HighlightStyle {
                    color: Some(color),
                    font_weight: Some(gpui::FontWeight::SEMIBOLD),
                    ..Default::default()
                },
            )
        })
        .collect()
}
