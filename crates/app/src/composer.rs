//! Floating message box. Sends to core in #main; in channels core isn't
//! wired to yet, sending is off and a note says so.

use std::{
    collections::HashMap,
    ops::Range,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chaos_core::emoji::{self, EmojiMatch};
use gpui::{
    Animation, AnimationExt, Context, Entity, Focusable, Hsla, KeyBinding, KeyDownEvent,
    SharedString, StyledText, Subscription, Task, TextRun, TextStyle, Window, actions, div, linear,
    prelude::*, px,
};
use model::{Id, ReplyRef, pane::author_name};
use ui::{
    ActiveTheme, Elevation, Ellipsis, IconName, TextInput, TextInputEvent, Tooltip, TypeStyleExt,
    icon,
    tokens::{icon as icons, radius, size, space, text},
    truncated,
};

use crate::mention_highlight::{self, KnownMentions};
use crate::store::{ChatStore, MentionCandidate, SendRoute, StoreEvent, UNWIRED_NOTE};

actions!(composer, [Cancel, AcceptSuggestion]);

/// Key context of the composer, around its text field.
pub const KEY_CONTEXT: &str = "Composer";

/// The composer's own keys. Enter and shift-enter are the text field's (see
/// `ui::TextInput::multiline`); Esc backs out of whatever the composer is in
/// the middle of, never the typed text. Tab accepts an open suggestion.
pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("escape", Cancel, Some(KEY_CONTEXT)),
        KeyBinding::new("tab", AcceptSuggestion, Some(KEY_CONTEXT)),
    ]
}

/// What Esc does in the composer, innermost first.
#[derive(Debug, PartialEq, Eq)]
enum Escape {
    /// Close an open suggestion popup and keep the reply.
    DismissSuggestions,
    /// Drop the reply reference. The typed text stays.
    CancelReply,
    /// Nothing to back out of: let Esc go on to whatever is behind.
    Propagate,
}

/// Esc precedence (STRM-576): a suggestion popup first, then the reply.
/// Edit mode (Up to edit your last message) slots in after the popup when it
/// exists. Text is never cleared.
fn escape(suggestions_open: bool, replying: bool) -> Escape {
    if suggestions_open {
        Escape::DismissSuggestions
    } else if replying {
        Escape::CancelReply
    } else {
        Escape::Propagate
    }
}

/// Whether Enter (or the send button) should post `text`: an empty or
/// whitespace-only composer sends nothing.
fn sendable(text: &str) -> bool {
    !text.trim().is_empty()
}

/// Core echoes the posted text trimmed; compare against that.
fn sent_body(text: &str) -> &str {
    text.trim()
}

/// Verbs cycled in the working indicator (STRM-662). Decorative only.
const WORKING_WORDS: &[&str] = &[
    "working",
    "pondering",
    "thinking",
    "cooking",
    "brewing",
    "tinkering",
    "crafting",
    "scheming",
    "mulling",
    "noodling",
];

const WORKING_WORD_INTERVAL: Duration = Duration::from_secs(5);

fn working_indicator_label(agents: &[(std::sync::Arc<str>, &'static str)], word: &str) -> String {
    match agents {
        [] => String::new(),
        [(name, _)] => format!("{name} is {word}…"),
        [(a, _), (b, _)] => format!("{a} and {b} are {word}…"),
        [all @ .., last] => {
            let names = all
                .iter()
                .map(|(n, _)| n.as_ref())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{names}, and {} are {word}…", last.0)
        }
    }
}

fn styled_working_text(
    label: &str,
    delta: f32,
    blur: f32,
    base_style: &TextStyle,
    base: Hsla,
    bright: Hsla,
) -> StyledText {
    let label_s: SharedString = label.to_string().into();
    let chars: Vec<(usize, char)> = label_s.char_indices().collect();
    if chars.is_empty() {
        return StyledText::new(label_s);
    }
    let n = chars.len() as f32;
    let center = -0.25 + delta * 1.5;
    let mut runs: Vec<TextRun> = Vec::with_capacity(chars.len());
    for (i, &(byte_ix, _ch)) in chars.iter().enumerate() {
        let next = chars.get(i + 1).map(|(ix, _)| *ix).unwrap_or(label_s.len());
        let len = next - byte_ix;
        let p = (i as f32 + 0.5) / n;
        let d = (p - center).abs();
        let highlight = (1.0 - (d / 0.18).min(1.0)).powf(1.8);
        let pulse = 0.55 + 0.45 * (1.0 - (2.0 * (delta - 0.5)).abs());
        let mix = (highlight * 0.85 + pulse * 0.15).clamp(0.0, 1.0);
        let mut color = base;
        color.h = base.h + (bright.h - base.h) * mix;
        color.s = base.s + (bright.s - base.s) * mix;
        color.l = base.l + (bright.l - base.l) * mix;
        let alpha = (0.45 + 0.55 * mix) * (1.0 - blur * 0.35);
        color.a = alpha.clamp(0.0, 1.0);
        runs.push(TextRun {
            len,
            font: base_style.font(),
            color,
            background_color: base_style.background_color,
            underline: base_style.underline,
            strikethrough: base_style.strikethrough,
        });
    }
    StyledText::new(label_s).with_runs(runs)
}

fn working_indicator_element(
    label: &str,
    delta: f32,
    base_style: &TextStyle,
    base: Hsla,
    bright: Hsla,
) -> impl IntoElement {
    // STRM-662: blur the word as it changes (blur-in at start of cycle, blur-out at end).
    let blur = if delta < 0.14 {
        1.0 - (delta / 0.14)
    } else if delta > 0.86 {
        (delta - 0.86) / 0.14
    } else {
        0.0
    }
    .clamp(0.0, 1.0);

    let main = styled_working_text(label, delta, blur, base_style, base, bright);
    if blur <= 0.01 {
        return div().child(main).into_any_element();
    }

    let offsets: [(f32, f32, f32); 6] = [
        (-1.6, 0.0, 0.16),
        (1.6, 0.0, 0.16),
        (0.0, -1.2, 0.14),
        (0.0, 1.2, 0.14),
        (-1.0, -0.8, 0.10),
        (1.0, 0.8, 0.10),
    ];
    let mut ghost_colors = base;
    ghost_colors.a = (base.a.max(0.55)) * blur;
    let mut layers = div().relative().child(main);
    for (i, (dx, dy, a)) in offsets.iter().enumerate() {
        let mut gc = ghost_colors;
        gc.a = (a * blur).clamp(0.0, 0.4);
        // approximate blur with faint offset copies of the same label
        let ghost = styled_working_text(label, delta, blur, base_style, gc, gc);
        layers = layers.child(
            div()
                .id(("working-blur", i))
                .absolute()
                .top(px(*dy))
                .left(px(*dx))
                .child(ghost),
        );
    }
    div().child(layers).into_any_element()
}

fn echo_matches_in(
    log: Option<&model::ChannelLog>,
    range: std::ops::Range<usize>,
    sent: &str,
) -> bool {
    let Some(log) = log else {
        return false;
    };
    range
        .filter_map(|ix| log.get(ix))
        .any(|m| matches!(m.author, model::Author::Human) && m.body.as_ref() == sent_body(sent))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Draft {
    text: SharedString,
    selection: Range<usize>,
    reversed: bool,
    reply: Option<ReplyRef>,
}

impl Draft {
    fn empty() -> Self {
        Self {
            text: SharedString::default(),
            selection: 0..0,
            reversed: false,
            reply: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.text.as_ref().trim().is_empty() && self.reply.is_none()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct UsageStat {
    count: u32,
    last_used: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MentionState {
    /// Byte range in the draft text to replace: `@` through the mention token.
    range: Range<usize>,
    /// Text typed after `@` up to the cursor, used for filtering.
    query: String,
    matches: Vec<MentionCandidate>,
    selected: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EmojiState {
    /// Byte range to replace: `:` through the shortcode token/query.
    range: Range<usize>,
    /// Text typed after `:` up to the cursor.
    query: String,
    matches: Vec<EmojiMatch>,
    selected: usize,
}

fn mention_query(text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    let at = before.rfind('@')?;
    let prefix = &before[at + 1..];
    if prefix.chars().any(char::is_whitespace) {
        return None;
    }
    if at > 0 {
        let prev = before[..at].chars().last()?;
        if !(prev.is_whitespace() || "([{\"'".contains(prev)) {
            return None;
        }
    }
    let after = &text[cursor..];
    let suffix_len = after
        .chars()
        .take_while(|c| !c.is_whitespace())
        .map(char::len_utf8)
        .sum::<usize>();
    let end = cursor + suffix_len;
    Some((at..end, prefix.to_string()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MatchRank {
    Exact = 0,
    Prefix = 1,
    WordBoundary = 2,
    Subsequence = 3,
}

fn subsequence_positions(hay: &[char], needle: &[char], start: usize) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return Some((start, start));
    }
    let mut i = start;
    let mut j = 0usize;
    let mut first = None;
    let mut last = start;
    while i < hay.len() && j < needle.len() {
        if hay[i].eq_ignore_ascii_case(&needle[j]) {
            if first.is_none() {
                first = Some(i);
            }
            last = i;
            j += 1;
        }
        i += 1;
    }
    if j == needle.len() {
        Some((first.unwrap_or(start), last + 1))
    } else {
        None
    }
}

fn subsequence_score(haystack: &str, needle: &str) -> Option<(usize, usize, usize)> {
    if needle.is_empty() {
        return Some((0, 0, 0));
    }
    let h: Vec<char> = haystack.chars().collect();
    let n: Vec<char> = needle.chars().collect();
    let mut best: Option<(usize, usize, usize)> = None; // (start, gap_penalty, boundary_penalty)
    for start in 0..h.len() {
        if !h[start].eq_ignore_ascii_case(&n[0]) {
            continue;
        }
        if let Some((s, end)) = subsequence_positions(&h, &n, start) {
            let gap_penalty = (end - s).saturating_sub(n.len());
            let boundary_penalty = if s == 0
                || !h[s - 1].is_ascii_alphanumeric()
                || (h[s - 1].is_ascii_lowercase() && h[s].is_ascii_uppercase())
            {
                0
            } else {
                1
            };
            let cand = (s, gap_penalty, boundary_penalty);
            if best.is_none_or(|b| cand < b) {
                best = Some(cand);
            }
        }
    }
    best
}

fn field_match_rank(field: &str, q: &str) -> Option<(MatchRank, usize, usize, usize)> {
    if q.is_empty() {
        return Some((MatchRank::Prefix, 0, 0, 0));
    }
    let f = field.to_ascii_lowercase();
    if f == q {
        return Some((MatchRank::Exact, 0, 0, 0));
    }
    if f.starts_with(q) {
        return Some((MatchRank::Prefix, 0, 0, 0));
    }
    if let Some(pos) = f.find(q) {
        let boundary = pos == 0
            || !field
                .as_bytes()
                .get(pos.saturating_sub(1))
                .is_some_and(|b| b.is_ascii_alphanumeric())
            || (field[..pos]
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_lowercase())
                && field[pos..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase()));
        let rank = if boundary {
            MatchRank::WordBoundary
        } else {
            MatchRank::Subsequence
        };
        return Some((rank, pos, 0, 0));
    }
    subsequence_score(&f, q).map(|(pos, gaps, bmiss)| (MatchRank::Subsequence, pos, gaps, bmiss))
}

fn best_mention_rank(
    candidate: &MentionCandidate,
    q: &str,
) -> Option<(MatchRank, usize, usize, usize, usize)> {
    // (rank, field_prio, pos, penalty, extra_for_stability)
    let mut best: Option<(MatchRank, usize, usize, usize, usize)> = None;
    let fields = std::iter::once((0usize, candidate.handle.as_ref()))
        .chain(std::iter::once((1usize, candidate.name.as_ref())))
        .chain(candidate.nicknames.iter().map(|n| (2usize, n.as_ref())));
    for (prio, field) in fields {
        if let Some((rank, pos, penalty, bmiss)) = field_match_rank(field, q) {
            let cand = (rank, prio, pos, penalty, bmiss);
            if best.is_none_or(|b| cand < b) {
                best = Some(cand);
            }
        }
    }
    best
}

type MentionScore = (
    (MatchRank, usize, usize, usize, usize),
    u32, // frequency
    u64, // age (smaller = more recent)
);

fn filter_mentions(
    candidates: &[MentionCandidate],
    query: &str,
    usage: &std::collections::HashMap<String, UsageStat>,
    now: u64,
) -> Vec<MentionCandidate> {
    let q = query.trim_start_matches('@').to_ascii_lowercase();
    let mut scored: Vec<(MentionCandidate, MentionScore)> = candidates
        .iter()
        .filter_map(|c| {
            best_mention_rank(c, &q).map(|rank| {
                let key = c.handle.to_ascii_lowercase();
                let u = usage.get(&key).copied().unwrap_or_default();
                let age = now.saturating_sub(u.last_used);
                (c.clone(), (rank, u.count, age))
            })
        })
        .collect();
    scored.sort_by(|a, b| {
        a.1.0
            .cmp(&b.1.0) // text match quality
            .then_with(|| b.1.1.cmp(&a.1.1)) // higher frequency
            .then_with(|| a.1.2.cmp(&b.1.2)) // more recent (smaller age)
            .then_with(|| {
                a.0.handle
                    .to_ascii_lowercase()
                    .cmp(&b.0.handle.to_ascii_lowercase())
            })
    });
    scored.into_iter().map(|(c, _)| c).collect()
}

fn is_shortcode_query_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+')
}

fn in_inline_code(text: &str, byte: usize) -> bool {
    let byte = byte.min(text.len());
    // Simple heuristic: inside an inline code span delimited by backticks on
    // the same line (no fenced blocks in the composer input).
    let line_start = text[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let mut ticks = 0usize;
    let mut i = line_start;
    while i < byte {
        let ch = text[i..].chars().next().unwrap();
        if ch == '`' {
            ticks += 1;
        }
        i += ch.len_utf8();
    }
    ticks % 2 == 1
}

fn emoji_query(text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];

    // Detect a completed shortcode token `:name:` when the cursor is at or just
    // after its closing colon.
    if before.ends_with(':') && before.len() >= 3 {
        let inner_end = before.len() - 1;
        if let Some(open) = before[..inner_end].rfind(':') {
            let name = &before[open + 1..inner_end];
            if !name.is_empty() && name.chars().all(is_shortcode_query_char) {
                if in_inline_code(text, open) {
                    return None;
                }
                let boundary_ok = if open == 0 {
                    true
                } else if let Some(prev) = before[..open].chars().last() {
                    prev != '/' && (prev.is_whitespace() || "([{\"'!?,.;".contains(prev))
                } else {
                    false
                };
                if boundary_ok {
                    return Some((open..before.len(), name.to_string()));
                }
            }
        }
    }

    let colon = before.rfind(':')?;
    let raw_prefix = &before[colon + 1..];
    if raw_prefix.is_empty() || !raw_prefix.chars().all(is_shortcode_query_char) {
        return None;
    }
    if in_inline_code(text, colon) {
        return None;
    }
    if colon > 0 {
        let prev = before[..colon].chars().last()?;
        // Avoid `://` in URLs and other cases where the colon is clearly part
        // of surrounding syntax rather than starting an emoji shortcode.
        if prev == '/' {
            return None;
        }
        // Keep ordinary punctuation usable: require a token boundary like
        // mentions do (start, whitespace, or common openers).
        if !(prev.is_whitespace() || "([{\"'!?,.;".contains(prev)) {
            return None;
        }
    }
    let after = &text[cursor..];
    let suffix_len = after
        .chars()
        .take_while(|&c| is_shortcode_query_char(c))
        .map(char::len_utf8)
        .sum::<usize>();
    let mut end = cursor + suffix_len;
    // If there's an immediate closing colon after the token, include it in the
    // replacement range so completing `:thumbs_up:` works naturally.
    if text[end..].starts_with(':') {
        end += 1;
    }
    Some((colon..end, raw_prefix.to_string()))
}

pub struct Composer {
    store: Entity<ChatStore>,
    input: Entity<TextInput>,
    /// The channel the composer is showing a draft for.
    channel: Id,
    /// "Reply" in the pane: the message the next send answers.
    reply: Option<ReplyRef>,
    drafts: HashMap<Id, Draft>,
    /// A core send in flight: restored if the send fails.
    pending: Option<(Id, Draft)>,
    mention: Option<MentionState>,
    emoji: Option<EmojiState>,
    mention_usage: HashMap<String, UsageStat>,
    emoji_usage: HashMap<String, UsageStat>,
    usage_path: Option<PathBuf>,
    working_word_index: usize,
    working_word_nonce: u64,
    working_timer_nonce: u64,
    _working_timer: Option<Task<()>>,
    _subscriptions: [Subscription; 4],
}

const SUGGESTION_USAGE_SCHEMA_VERSION: u64 = 1;
const SUGGESTION_USAGE_MAX_ENTRIES: usize = 500;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Emoji the user has picked before come first (more uses, then more
/// recent), whether or not a shortcode was typed. A typed query only ever
/// reorders `emoji::suggest`'s matches, so relevance still decides what shows;
/// unused matches keep their original order (the sort is stable).
fn rank_emoji_matches(matches: &mut [EmojiMatch], usage: &HashMap<String, UsageStat>) {
    matches.sort_by(|a, b| {
        let ua = usage.get(a.emoji.as_ref()).copied().unwrap_or_default();
        let ub = usage.get(b.emoji.as_ref()).copied().unwrap_or_default();
        ub.count
            .cmp(&ua.count)
            .then_with(|| ub.last_used.cmp(&ua.last_used))
    });
}

fn bounded_count(value: Option<&serde_json::Value>) -> u32 {
    match value.and_then(|v| v.as_u64()) {
        Some(n) => n.min(u32::MAX as u64) as u32,
        None => 0,
    }
}

fn bounded_time(value: Option<&serde_json::Value>) -> u64 {
    value.and_then(|v| v.as_u64()).unwrap_or(0)
}

fn parse_usage_map(value: Option<&serde_json::Value>) -> HashMap<String, UsageStat> {
    let mut entries: Vec<(String, UsageStat)> = Vec::new();
    if let Some(m) = value.and_then(|v| v.as_object()) {
        for (k, v) in m {
            entries.push((
                k.clone(),
                UsageStat {
                    count: bounded_count(v.get("count")),
                    last_used: bounded_time(v.get("last_used")),
                },
            ));
        }
    }
    // Bound persisted state: keep most relevant entries (count, then recency).
    entries.sort_by(|a, b| {
        b.1.count
            .cmp(&a.1.count)
            .then_with(|| b.1.last_used.cmp(&a.1.last_used))
            .then_with(|| a.0.cmp(&b.0))
    });
    entries.truncate(SUGGESTION_USAGE_MAX_ENTRIES);
    entries.into_iter().collect()
}

fn usage_map_to_json(usage: &HashMap<String, UsageStat>) -> serde_json::Value {
    let mut entries: Vec<(&String, &UsageStat)> = usage.iter().collect();
    entries.sort_by(|a, b| {
        b.1.count
            .cmp(&a.1.count)
            .then_with(|| b.1.last_used.cmp(&a.1.last_used))
            .then_with(|| a.0.cmp(b.0))
    });
    entries.truncate(SUGGESTION_USAGE_MAX_ENTRIES);
    let mut m = serde_json::Map::new();
    for (k, v) in entries {
        m.insert(
            k.clone(),
            serde_json::json!({ "count": v.count, "last_used": v.last_used }),
        );
    }
    serde_json::Value::Object(m)
}

fn load_suggestion_usage(
    path: Option<&std::path::Path>,
) -> (HashMap<String, UsageStat>, HashMap<String, UsageStat>) {
    let Some(path) = path else {
        return (HashMap::new(), HashMap::new());
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return (HashMap::new(), HashMap::new());
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (HashMap::new(), HashMap::new());
    };
    // Unknown/missing schema version: best-effort parse for forward/backward tolerance.
    let _version = value.get("version").and_then(|v| v.as_u64());
    (
        parse_usage_map(value.get("mentions")),
        parse_usage_map(value.get("emoji")),
    )
}

fn save_suggestion_usage(
    path: Option<&std::path::Path>,
    mentions: &HashMap<String, UsageStat>,
    emoji: &HashMap<String, UsageStat>,
) {
    let Some(path) = path else { return };
    let root = serde_json::json!({
        "version": SUGGESTION_USAGE_SCHEMA_VERSION,
        "mentions": usage_map_to_json(mentions),
        "emoji": usage_map_to_json(emoji),
    });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(text) = serde_json::to_string_pretty(&root) else {
        return;
    };
    // Atomic-ish write: write temp then rename to avoid truncating a valid file on failure.
    let tmp = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), now_secs()));
    let write_ok = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();
    if write_ok.is_ok() {
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Rows the composer grows to before it scrolls.
const MAX_LINES: usize = 8;

fn placeholder(store: &ChatStore) -> String {
    format!("Message #{}", store.selected_channel().name)
}

impl Composer {
    pub fn new(
        store: Entity<ChatStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
        usage_path: Option<PathBuf>,
    ) -> Self {
        // Enter sends, shift-enter starts a new line, and the field grows with
        // its text (see `TextInput::multiline`).
        let input =
            cx.new(|cx| TextInput::new(placeholder(store.read(cx)), cx).multiline(MAX_LINES));
        {
            let store_for_highlight = store.clone();
            input.update(cx, |input, cx| {
                input.set_highlighter(
                    move |text, appearance, app| {
                        let store = store_for_highlight.read(app);
                        let mut known = KnownMentions::new();
                        for member in store.mention_candidates() {
                            known.insert_member(
                                member.handle.as_ref(),
                                member.handle.as_ref(),
                                &member.nicknames,
                            );
                        }
                        known.insert_broadcasts();
                        mention_highlight::mention_highlights(text, &known, appearance)
                    },
                    cx,
                );
            });
        }
        window.focus(&input.focus_handle(cx));
        let channel = store.read(cx).selected();
        let subscriptions = [
            cx.subscribe(&input, |this, _, event, cx| match event {
                TextInputEvent::Submit => {
                    if this.suggestions_open() {
                        this.accept_suggestion(cx);
                    } else {
                        this.send(cx);
                    }
                }
                TextInputEvent::Changed => this.refresh_suggestions(cx),
            }),
            cx.observe(&input, |this, _, cx| this.refresh_suggestions(cx)),
            cx.observe(&store, |this, _, cx| this.sync_working_indicator(cx)),
            cx.subscribe(&store, |this, store, event, cx| match event {
                StoreEvent::Selected => {
                    let next = store.read(cx).selected();
                    this.switch_channel(next, store, cx);
                }
                StoreEvent::SendFailed { channel, .. } => {
                    if this.pending.as_ref().is_some_and(|(ch, _)| ch == channel) {
                        let (_, draft) = this.pending.take().unwrap();
                        // Keep the text if the send failed.
                        this.drafts.insert(*channel, draft.clone());
                        if this.channel == *channel && this.input.read(cx).text().is_empty() {
                            this.reply = draft.reply.clone();
                            this.input.update(cx, |input, cx| {
                                input.restore_draft_state(
                                    draft.text,
                                    draft.selection,
                                    draft.reversed,
                                    cx,
                                );
                            });
                        }
                        cx.notify();
                    }
                }
                StoreEvent::Appended { channel, range } => {
                    // A successful core send shows up as an appended message.
                    let matched = this.pending.as_ref().and_then(|(ch, draft)| {
                        (*ch == *channel
                            && echo_matches_in(
                                store.read(cx).log(*channel),
                                range.clone(),
                                draft.text.as_ref(),
                            ))
                        .then(|| draft.text.as_ref().to_string())
                    });
                    if let Some(sent_text) = matched {
                        this.pending = None;
                        this.drafts.remove(channel);
                        if this.channel == *channel {
                            if this.input.read(cx).text().as_ref() == sent_text.as_str() {
                                this.input.update(cx, |input, cx| input.clear(cx));
                                this.reply = None;
                            }
                            cx.notify();
                        }
                    }
                }
                _ => {}
            }),
        ];
        let (mention_usage, emoji_usage) = load_suggestion_usage(usage_path.as_deref());
        let mut this = Self {
            store,
            input,
            channel,
            reply: None,
            drafts: HashMap::new(),
            pending: None,
            mention: None,
            emoji: None,
            mention_usage,
            emoji_usage,
            usage_path,
            working_word_index: 0,
            working_word_nonce: 0,
            working_timer_nonce: 0,
            _working_timer: None,
            _subscriptions: subscriptions,
        };
        this.sync_working_indicator(cx);
        this
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.input.focus_handle(cx));
    }

    fn snapshot(&self, cx: &Context<Self>) -> Draft {
        let (text, selection, reversed) = self.input.read(cx).draft_state();
        Draft {
            text,
            selection,
            reversed,
            reply: self.reply.clone(),
        }
    }

    fn save_current(&mut self, cx: &Context<Self>) {
        let draft = self.snapshot(cx);
        if draft.is_empty() {
            self.drafts.remove(&self.channel);
        } else {
            self.drafts.insert(self.channel, draft);
        }
    }

    fn restore_for(&mut self, channel: Id, cx: &mut Context<Self>) {
        let draft = self
            .drafts
            .get(&channel)
            .cloned()
            .unwrap_or_else(Draft::empty);
        self.reply = draft.reply;
        self.input.update(cx, |input, cx| {
            input.restore_draft_state(draft.text, draft.selection, draft.reversed, cx);
        });
    }

    fn switch_channel(&mut self, next: Id, store: Entity<ChatStore>, cx: &mut Context<Self>) {
        if next == self.channel {
            return;
        }
        self.save_current(cx);
        self.channel = next;
        let placeholder = placeholder(store.read(cx));
        self.input
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        self.restore_for(next, cx);
        self.mention = None;
        self.emoji = None;
        self.refresh_suggestions(cx);
        self.sync_working_indicator(cx);
        cx.notify();
    }

    /// Starts a reply to a message (the pane's "Reply"): shows the reference
    /// above the field and hands the keyboard to the composer.
    pub fn set_reply(&mut self, reply: ReplyRef, window: &mut Window, cx: &mut Context<Self>) {
        self.reply = Some(reply);
        self.focus(window, cx);
        cx.notify();
    }

    /// True while a suggestion popup owns keyboard selection.
    fn suggestions_open(&self) -> bool {
        self.mention.is_some() || self.emoji.is_some()
    }

    fn sync_vertical_nav_suppression(&self, cx: &mut Context<Self>) {
        let open = self.suggestions_open();
        self.input.update(cx, |input, _| {
            input.set_suppress_vertical_navigation(open);
        });
    }

    /// Closes any open suggestion popup.
    fn dismiss_suggestions(&mut self, cx: &mut Context<Self>) {
        if self.mention.take().is_some() || self.emoji.take().is_some() {
            self.sync_vertical_nav_suppression(cx);
            cx.notify();
        }
    }

    fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let (text, selection, _) = self.input.read(cx).draft_state();
        if selection.start != selection.end {
            let changed = self.mention.take().is_some() || self.emoji.take().is_some();
            if changed {
                self.sync_vertical_nav_suppression(cx);
                cx.notify();
            }
            return;
        }
        let cursor = selection.end.min(text.len());
        // Prefer mention suggestions when typing `@...`; otherwise try emoji.
        if self.refresh_mention(text.as_ref(), cursor, cx) {
            self.sync_vertical_nav_suppression(cx);
            return;
        }
        self.refresh_emoji(text.as_ref(), cursor, cx);
        self.sync_vertical_nav_suppression(cx);
    }

    fn refresh_mention(&mut self, text: &str, cursor: usize, cx: &mut Context<Self>) -> bool {
        let Some((range, query)) = mention_query(text, cursor) else {
            if self.mention.take().is_some() {
                self.emoji = None;
                cx.notify();
                return false;
            }
            return false;
        };
        let candidates = self.store.read(cx).mention_candidates();
        let matches = filter_mentions(&candidates, &query, &self.mention_usage, now_secs());
        if matches.is_empty() {
            let changed = self.mention.take().is_some();
            self.emoji = None;
            if changed {
                cx.notify();
            }
            return false;
        }
        if self.emoji.take().is_some() {
            // Mention takes precedence over emoji in overlapping contexts.
        }
        let selected = self
            .mention
            .as_ref()
            .filter(|m| m.query == query)
            .and_then(|m| {
                m.matches
                    .get(m.selected)
                    .and_then(|keep| matches.iter().position(|c| c.handle == keep.handle))
            })
            .unwrap_or(0);
        let next = MentionState {
            range,
            query,
            matches,
            selected,
        };
        if self.mention.as_ref() == Some(&next) {
            return true;
        }
        self.mention = Some(next);
        cx.notify();
        true
    }

    fn refresh_emoji(&mut self, text: &str, cursor: usize, cx: &mut Context<Self>) {
        let Some((range, query)) = emoji_query(text, cursor) else {
            if self.emoji.take().is_some() {
                cx.notify();
            }
            return;
        };
        let mut matches = emoji::suggest(&query, 12);
        if matches.is_empty() {
            if self.emoji.take().is_some() {
                cx.notify();
            }
            return;
        }
        rank_emoji_matches(&mut matches, &self.emoji_usage);
        let selected = self
            .emoji
            .as_ref()
            .filter(|m| m.query == query)
            .and_then(|m| {
                m.matches.get(m.selected).and_then(|keep| {
                    matches
                        .iter()
                        .position(|c| c.shortcode == keep.shortcode && c.emoji == keep.emoji)
                })
            })
            .unwrap_or(0);
        let next = EmojiState {
            range,
            query,
            matches,
            selected,
        };
        if self.emoji.as_ref() == Some(&next) {
            return;
        }
        self.emoji = Some(next);
        cx.notify();
    }

    fn move_suggestion_selection(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        if let Some(state) = &mut self.emoji {
            let len = state.matches.len();
            if len == 0 {
                return false;
            }
            let cur = state.selected as isize;
            state.selected = (cur + delta).rem_euclid(len as isize) as usize;
            cx.notify();
            return true;
        }
        let Some(state) = &mut self.mention else {
            return false;
        };
        let len = state.matches.len();
        if len == 0 {
            return false;
        }
        let cur = state.selected as isize;
        state.selected = (cur + delta).rem_euclid(len as isize) as usize;
        cx.notify();
        true
    }

    fn accept_suggestion(&mut self, cx: &mut Context<Self>) -> bool {
        if self.emoji.is_some() {
            return self.accept_emoji(cx);
        }
        self.accept_mention(cx)
    }

    fn accept_mention(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.mention.clone() else {
            return false;
        };
        let Some(candidate) = state.matches.get(state.selected) else {
            self.mention = None;
            return false;
        };
        let (text, selection, _reversed) = self.input.read(cx).draft_state();
        if selection.start != selection.end {
            self.refresh_suggestions(cx);
            return false;
        }
        let cursor = selection.end.min(text.len());
        if cursor < state.range.start
            || cursor > state.range.end
            || state.range.end > text.len()
            || !text.is_char_boundary(state.range.start)
            || !text.is_char_boundary(state.range.end)
            || !text.is_char_boundary(cursor)
        {
            self.refresh_suggestions(cx);
            return false;
        }
        let handle = candidate.handle.to_string();
        self.record_mention_use(&handle);
        let mut new = String::with_capacity(text.len() - state.range.len() + handle.len() + 2);
        new.push_str(&text.as_ref()[..state.range.start]);
        new.push('@');
        new.push_str(&handle);
        let insert_end = new.len();
        new.push(' ');
        new.push_str(&text.as_ref()[state.range.end..]);
        let caret = insert_end + 1;
        self.input.update(cx, |input, cx| {
            input.restore_draft_state(SharedString::from(new), caret..caret, false, cx);
        });
        self.mention = None;
        self.refresh_suggestions(cx);
        true
    }

    fn accept_emoji(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.emoji.clone() else {
            return false;
        };
        let Some(candidate) = state.matches.get(state.selected) else {
            self.emoji = None;
            return false;
        };
        let (text, selection, _reversed) = self.input.read(cx).draft_state();
        if selection.start != selection.end {
            self.refresh_suggestions(cx);
            return false;
        }
        let cursor = selection.end.min(text.len());
        if cursor < state.range.start
            || cursor > state.range.end
            || state.range.end > text.len()
            || !text.is_char_boundary(state.range.start)
            || !text.is_char_boundary(state.range.end)
            || !text.is_char_boundary(cursor)
        {
            self.refresh_suggestions(cx);
            return false;
        }
        let emoji = candidate.emoji.as_ref();
        self.record_emoji_use(emoji);
        let mut new = String::with_capacity(text.len() - state.range.len() + emoji.len() + 1);
        new.push_str(&text.as_ref()[..state.range.start]);
        new.push_str(emoji);
        let insert_end = new.len();
        new.push(' ');
        new.push_str(&text.as_ref()[state.range.end..]);
        let caret = insert_end + 1;
        self.input.update(cx, |input, cx| {
            input.restore_draft_state(SharedString::from(new), caret..caret, false, cx);
        });
        self.emoji = None;
        self.refresh_suggestions(cx);
        true
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        match escape(self.suggestions_open(), self.reply.is_some()) {
            Escape::DismissSuggestions => self.dismiss_suggestions(cx),
            Escape::CancelReply => {
                self.reply = None;
                self.save_current(cx);
                cx.notify();
            }
            Escape::Propagate => cx.propagate(),
        }
    }

    fn accept_suggestion_action(
        &mut self,
        _: &AcceptSuggestion,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.accept_suggestion(cx) {
            cx.propagate();
        }
    }

    fn record_mention_use(&mut self, handle: &str) {
        let key = handle.to_ascii_lowercase();
        let entry = self.mention_usage.entry(key).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.last_used = now_secs();
        save_suggestion_usage(
            self.usage_path.as_deref(),
            &self.mention_usage,
            &self.emoji_usage,
        );
    }

    fn record_emoji_use(&mut self, emoji: &str) {
        let key = emoji.to_string();
        let entry = self.emoji_usage.entry(key).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.last_used = now_secs();
        save_suggestion_usage(
            self.usage_path.as_deref(),
            &self.mention_usage,
            &self.emoji_usage,
        );
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().clone();
        if !sendable(&text) || self.unwired(cx) {
            return;
        }
        let route = self.store.read(cx).send_route();
        let reply = self.reply.take();
        // Keep a copy so a failed send can be restored.
        let (draft_text, selection, reversed) = self.input.read(cx).draft_state();
        let recovery = Draft {
            text: draft_text,
            selection,
            reversed,
            reply: reply.clone(),
        };
        self.store
            .update(cx, |store, cx| store.post_from_human(&text, reply, cx));
        self.input.update(cx, |input, cx| input.clear(cx));
        self.reply = None;
        self.drafts.remove(&self.channel);
        if route == SendRoute::Core {
            // Keep the draft until the send is known to have failed; core
            // reports success by echoing the message back.
            self.pending = Some((self.channel, recovery));
        }
        cx.notify();
    }

    /// The selected channel isn't wired to core, so sending is off.
    fn unwired(&self, cx: &Context<Self>) -> bool {
        self.store.read(cx).send_route() == SendRoute::Unwired
    }

    fn has_working_agents(&self, cx: &Context<Self>) -> bool {
        let store = self.store.read(cx);
        !store.working_agents(store.selected()).is_empty()
    }

    fn sync_working_indicator(&mut self, cx: &mut Context<Self>) {
        if self.has_working_agents(cx) {
            if self._working_timer.is_none() {
                self.working_word_index = 0;
                self.start_working_timer(cx);
            }
            cx.notify();
        } else {
            self.working_timer_nonce = self.working_timer_nonce.wrapping_add(1);
            self.working_word_nonce = self.working_word_nonce.wrapping_add(1);
            self._working_timer = None;
            self.working_word_index = 0;
        }
    }

    fn start_working_timer(&mut self, cx: &mut Context<Self>) {
        self.working_timer_nonce = self.working_timer_nonce.wrapping_add(1);
        let timer_nonce = self.working_timer_nonce;
        self.working_word_nonce = self.working_word_nonce.wrapping_add(1);
        self._working_timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(WORKING_WORD_INTERVAL).await;
                let keep = this
                    .update(cx, |this, cx| {
                        if this.working_timer_nonce != timer_nonce || !this.has_working_agents(cx) {
                            return false;
                        }
                        this.working_word_index =
                            (this.working_word_index + 1) % WORKING_WORDS.len();
                        // Restart animation so the new word blurs in (STRM-662).
                        this.working_word_nonce = this.working_word_nonce.wrapping_add(1);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
    }

    fn working_indicator_label(&self, cx: &Context<Self>) -> Option<String> {
        let store = self.store.read(cx);
        let agents = store.working_agents(store.selected());
        if agents.is_empty() {
            return None;
        }
        let word = WORKING_WORDS[self.working_word_index % WORKING_WORDS.len()];
        Some(working_indicator_label(&agents, word))
    }

    fn render_reply(&self, reply: &ReplyRef, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = *cx.theme();
        let who = SharedString::from(author_name(self.store.read(cx).workspace(), reply.author));
        let hover = t.hover;
        div()
            .mx(space::X1)
            .mb(space::X2)
            .flex()
            .items_center()
            .gap(space::X2)
            .pl(space::X3)
            .pr(space::X1)
            .py(space::X1)
            .rounded(radius::MD)
            .bg(t.sunken)
            .border_l_2()
            .border_color(t.accent)
            .type_style(text::SMALL)
            .child(div().flex_none().text_color(t.muted).child("Replying to"))
            .child(
                div()
                    .flex_none()
                    .font_weight(text::SEMIBOLD)
                    .text_color(t.ink)
                    .child(who),
            )
            .child(
                div()
                    .flex_1()
                    .ellipsis()
                    .text_color(t.ink_soft)
                    .child(truncated(reply.quote.clone())),
            )
            .child(
                div()
                    .id("cancel-reply")
                    .size(size::CHIP)
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(radius::MD)
                    .hover(move |s| s.bg(hover))
                    .tooltip(Tooltip::text("Cancel reply"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.reply = None;
                        this.focus(window, cx);
                        this.save_current(cx);
                        cx.notify();
                    }))
                    .child(icon(IconName::X, icons::SM, t.muted)),
            )
    }

    fn render_suggestions(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if let Some(state) = &self.emoji {
            let t = *cx.theme();
            let hover = t.hover;
            let rows = state.matches.iter().enumerate().map(|(ix, m)| {
                let selected = ix == state.selected;
                let shortcode = format!(":{}:", m.shortcode);
                let idx = ix;
                div()
                    .id(("emoji", ix))
                    .px(space::X2)
                    .py(space::X1_5)
                    .rounded(radius::MD)
                    .bg(if selected { t.selected } else { t.surface })
                    .hover(move |s| s.bg(if selected { t.selected } else { hover }))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(space::X2)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(state) = &mut this.emoji {
                                state.selected = idx;
                            }
                            this.accept_suggestion(cx);
                        }),
                    )
                    .child(
                        div()
                            .type_style(text::BODY)
                            .child(SharedString::from(m.emoji.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .type_style(text::SMALL)
                                    .font_weight(text::SEMIBOLD)
                                    .text_color(t.ink)
                                    .child(shortcode),
                            )
                            .child(
                                div()
                                    .type_style(text::CAPTION)
                                    .text_color(t.muted)
                                    .child(SharedString::from(m.name.clone())),
                            ),
                    )
            });
            return Some(
                div()
                    .id("emoji-suggestions")
                    .w_full()
                    .p(space::X1)
                    .rounded(radius::MD)
                    .bg(t.surface)
                    .border_1()
                    .border_color(t.line)
                    .shadow(t.elevation(Elevation::Overlay))
                    .flex()
                    .flex_col()
                    .gap(space::X0_5)
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .children(rows)
                    .into_any_element(),
            );
        }

        let state = self.mention.as_ref()?;
        let t = *cx.theme();
        let hover = t.hover;
        let rows = state.matches.iter().enumerate().map(|(ix, m)| {
            let selected = ix == state.selected;
            let handle = format!("@{}", m.handle);
            let nick = if m.nicknames.is_empty() {
                None
            } else {
                Some(
                    m.nicknames
                        .iter()
                        .map(|n| format!("@{n}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            };
            let idx = ix;
            div()
                .id(("mention", ix))
                .px(space::X2)
                .py(space::X1_5)
                .rounded(radius::MD)
                .bg(if selected { t.selected } else { t.surface })
                .hover(move |s| s.bg(if selected { t.selected } else { hover }))
                .cursor_pointer()
                .flex()
                .flex_col()
                .gap(space::X0_5)
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        if let Some(state) = &mut this.mention {
                            state.selected = idx;
                        }
                        this.accept_suggestion(cx);
                    }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::X2)
                        .child(
                            div()
                                .type_style(text::SMALL)
                                .font_weight(text::SEMIBOLD)
                                .text_color(t.ink)
                                .child(handle),
                        )
                        .child(
                            div()
                                .type_style(text::SMALL)
                                .text_color(t.muted)
                                .child(SharedString::from(m.name.clone())),
                        ),
                )
                .when_some(nick, |d, nick| {
                    d.child(
                        div()
                            .type_style(text::CAPTION)
                            .text_color(t.muted)
                            .child(nick),
                    )
                })
        });
        Some(
            div()
                .id("mention-suggestions")
                .w_full()
                .p(space::X1)
                .rounded(radius::MD)
                .bg(t.surface)
                .border_1()
                .border_color(t.line)
                .shadow(t.elevation(Elevation::Overlay))
                .flex()
                .flex_col()
                .gap(space::X0_5)
                .max_h(px(320.))
                .overflow_y_scroll()
                .children(rows)
                .into_any_element(),
        )
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let unwired = self.unwired(cx);
        let can_send = !unwired && sendable(self.input.read(cx).text());
        let focused = self.input.focus_handle(cx).contains_focused(window, cx);
        let hover = t.hover;

        let working = self.working_indicator_label(cx);

        let box_el = div()
            .w_full()
            .flex()
            .flex_col()
            .rounded(radius::FIELD)
            .border_1()
            .border_color(if focused { t.line_strong } else { t.line })
            .bg(t.surface)
            .px(space::X2)
            .pt(space::X3)
            .pb(space::X2)
            .shadow(t.elevation(Elevation::Float))
            .when(unwired, |d| {
                d.child(
                    div()
                        .px(space::X2)
                        .mb(space::X1_5)
                        .type_style(text::SMALL)
                        .text_color(t.muted)
                        .child(UNWIRED_NOTE),
                )
            })
            .when_some(self.reply.clone(), |d, reply| {
                d.child(self.render_reply(&reply, cx))
            })
            .child(
                div()
                    .px(space::X2)
                    .type_style(text::BODY)
                    .text_color(t.ink)
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .mt(space::X1_5)
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .id("attach")
                            .size(size::BUTTON)
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius::MD)
                            .hover(move |s| s.bg(hover))
                            .tooltip(Tooltip::text("Attach"))
                            .child(icon(IconName::Plus, icons::MD, t.muted)),
                    )
                    .child(
                        div()
                            .id("send")
                            .size(size::BUTTON)
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius::MD)
                            // Disabled reads as a muted button, not a missing one.
                            .bg(if can_send { t.ink } else { t.faint })
                            .tooltip(Tooltip::text("Send"))
                            .on_click(cx.listener(|this, _, _, cx| this.send(cx)))
                            .child(icon(IconName::ArrowUp, icons::MD, t.surface)),
                    ),
            );

        let base_style = window.text_style();
        let nonce = self.working_word_nonce;
        div()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::accept_suggestion_action))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if !this.suggestions_open() {
                    return;
                }
                let key = event.keystroke.key.as_str();
                match key {
                    "up" if this.move_suggestion_selection(-1, cx) => cx.stop_propagation(),
                    "down" if this.move_suggestion_selection(1, cx) => cx.stop_propagation(),
                    _ => {}
                }
            }))
            .w_full()
            .relative()
            .flex()
            .flex_col()
            .gap(space::X1_5)
            .children(self.render_suggestions(cx).map(|el| {
                div()
                    .absolute()
                    .bottom_full()
                    .left_0()
                    .right_0()
                    .mb(space::X1)
                    .child(el)
            }))
            .when_some(working, |d, label| {
                let base = t.muted;
                let bright = t.ink;
                d.child(
                    div()
                        .px(space::X2)
                        .py(space::X0_5)
                        .type_style(text::SMALL)
                        .child(
                            div()
                                .id(("working-indicator", nonce as usize))
                                .with_animation(
                                    ("working-indicator-shimmer", nonce as usize),
                                    Animation::new(WORKING_WORD_INTERVAL)
                                        .repeat()
                                        .with_easing(linear),
                                    move |el, delta| {
                                        el.child(working_indicator_element(
                                            &label,
                                            delta,
                                            &base_style,
                                            base,
                                            bright,
                                        ))
                                    },
                                ),
                        ),
                )
            })
            .child(box_el)
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Action as _, KeyContext, Keymap, Keystroke};

    use super::*;

    /// The action `keys` runs with focus in the composer's text field, as
    /// GPUI's dispatcher would pick it from the whole app keymap.
    fn action_for(keys: &str) -> Option<&'static str> {
        let mut all = bindings();
        all.extend(ui::text_input::bindings());
        let keymap = Keymap::new(all);
        let contexts = [
            KeyContext::parse("Workspace").unwrap(),
            KeyContext::parse(KEY_CONTEXT).unwrap(),
            KeyContext::parse(ui::text_input::MULTILINE_KEY_CONTEXT).unwrap(),
        ];
        let keystroke = Keystroke::parse(keys).unwrap();
        let (matched, _) = keymap.bindings_for_input(&[keystroke], &contexts);
        matched.first().map(|binding| binding.action().name())
    }

    #[test]
    fn esc_in_the_composer_cancels() {
        assert_eq!(action_for("escape"), Some(Cancel.name()));
    }

    #[test]
    fn enter_and_shift_enter_still_reach_the_text_field() {
        assert_eq!(action_for("enter"), Some(ui::text_input::Submit.name()));
        assert_eq!(
            action_for("shift-enter"),
            Some(ui::text_input::Newline.name())
        );
    }

    #[test]
    fn esc_closes_suggestions_before_the_reply() {
        assert_eq!(escape(true, true), Escape::DismissSuggestions);
        assert_eq!(escape(true, false), Escape::DismissSuggestions);
    }

    #[test]
    fn esc_cancels_a_reply_when_nothing_is_open() {
        assert_eq!(escape(false, true), Escape::CancelReply);
    }

    #[test]
    fn esc_with_nothing_to_cancel_passes_through() {
        assert_eq!(escape(false, false), Escape::Propagate);
    }

    #[test]
    fn empty_or_whitespace_only_text_is_not_sent() {
        assert!(!sendable(""));
        assert!(!sendable("   "));
        assert!(!sendable("\n\n"));
        assert!(!sendable(" \t\n \r\n"));
        assert!(sendable("hi"));
        assert!(sendable("  hi\n"));
    }

    #[test]
    fn drafts_switch_and_restore_per_channel() {
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut drafts = HashMap::new();
        let a = Id(1);
        let b = Id(2);
        let draft_a = Draft {
            text: SharedString::from("hello"),
            selection: 2..5,
            reversed: false,
            reply: None,
        };

        // Save channel A, switch away, restore it.
        if draft_a.is_empty() {
            drafts.remove(&a);
        } else {
            drafts.insert(a, draft_a.clone());
        }
        let restored_a = drafts.get(&a).cloned().unwrap_or_else(Draft::empty);
        assert_eq!(restored_a, draft_a);

        // Empty draft is not kept.
        let empty = Draft::empty();
        if empty.is_empty() {
            drafts.remove(&b);
        } else {
            drafts.insert(b, empty.clone());
        }
        assert!(!drafts.contains_key(&b));
        assert_eq!(drafts.get(&b).cloned().unwrap_or_else(Draft::empty), empty);

        // Reply makes an otherwise empty text draft worth keeping.
        let reply = ReplyRef {
            message: Id(99),
            author: model::Author::Human,
            quote: Arc::from("quoted"),
        };
        let draft_reply = Draft {
            text: SharedString::default(),
            selection: 0..0,
            reversed: false,
            reply: Some(reply),
        };
        assert!(!draft_reply.is_empty());
        drafts.insert(b, draft_reply.clone());
        assert_eq!(
            drafts.get(&b).cloned().unwrap_or_else(Draft::empty),
            draft_reply
        );
    }

    #[test]
    fn core_echo_match_trims_and_scans_appended_range() {
        use std::sync::Arc;

        let mut log = model::ChannelLog::default();
        let human = |id: u32, body: &str| model::Message {
            id: Id(id),
            author: model::Author::Human,
            sent_at: 0,
            body: Arc::from(body),
            reply_to: None,
            stance: None,
        };
        let agent = |id: u32, body: &str| model::Message {
            id: Id(id),
            author: model::Author::Agent(Id(7)),
            sent_at: 0,
            body: Arc::from(body),
            reply_to: None,
            stance: None,
        };

        // Core stores trimmed body; draft may have trailing whitespace.
        let r1 = log.extend([human(1, "hello")]);
        assert!(echo_matches_in(Some(&log), r1.clone(), "hello  \n"));

        // If other messages land in the same appended range, still find the echo.
        let r2 = log.extend([agent(2, "reply"), human(3, "second")]);
        assert!(echo_matches_in(Some(&log), r2, "second\n"));

        // Different body does not match.
        assert!(!echo_matches_in(Some(&log), 0..log.len(), "other"));
    }

    #[test]
    fn mention_query_parses_at_token() {
        assert_eq!(
            mention_query("hello @cod", 10),
            Some((6..10, "cod".to_string()))
        );
        assert_eq!(mention_query("@al", 3), Some((0..3, "al".to_string())));
        assert_eq!(mention_query("email test@example.com", 20), None);
        // Cursor moved into the token: query is the prefix, range covers the whole mention token.
        assert_eq!(
            mention_query("hi @foo bar", 5),
            Some((3..7, "f".to_string()))
        );
        assert_eq!(mention_query("hello", 5), None);
        assert_eq!(mention_query("( @", 3), Some((2..3, String::new())));
        // Insertion/edit halfway through an existing mention keeps replacement over the full token.
        assert_eq!(
            mention_query("hi @claude there", 6),
            Some((3..10, "cl".to_string()))
        );
    }

    #[test]
    fn mention_filter_matches_handle_name_or_nickname() {
        let candidates = vec![
            MentionCandidate {
                handle: "claude".into(),
                name: "Claude Code".into(),
                nicknames: vec!["cx".into(), "cc".into()],
            },
            MentionCandidate {
                handle: "codex".into(),
                name: "Codex CLI".into(),
                nicknames: vec![],
            },
        ];
        let usage = std::collections::HashMap::new();
        let cla = filter_mentions(&candidates, "cla", &usage, 0);
        assert_eq!(cla.first().map(|c| c.handle.as_ref()), Some("claude"));
        assert!(!cla.iter().any(|c| c.handle.as_ref() == "codex"));
        let cli = filter_mentions(&candidates, "cli", &usage, 0);
        assert_eq!(cli.first().map(|c| c.handle.as_ref()), Some("codex"));
        assert!(!cli.iter().any(|c| c.handle.as_ref() == "claude"));
        let cx = filter_mentions(&candidates, "CX", &usage, 0);
        assert_eq!(cx.first().map(|c| c.handle.as_ref()), Some("claude"));
        assert_eq!(filter_mentions(&candidates, "", &usage, 0).len(), 2);
        assert!(filter_mentions(&candidates, "zzz", &usage, 0).is_empty());
    }

    #[test]
    fn mention_ranking_prefers_exact_prefix_then_subsequence() {
        let candidates = vec![
            MentionCandidate {
                handle: "albert".into(),
                name: "Albert".into(),
                nicknames: vec![],
            },
            MentionCandidate {
                handle: "al".into(),
                name: "Al".into(),
                nicknames: vec![],
            },
            MentionCandidate {
                handle: "gal".into(),
                name: "Gal".into(),
                nicknames: vec![],
            },
        ];
        let usage = std::collections::HashMap::new();
        let ranked = filter_mentions(&candidates, "al", &usage, 0);
        assert_eq!(ranked[0].handle.as_ref(), "al");
        assert_eq!(ranked[1].handle.as_ref(), "albert");
    }

    #[test]
    fn mention_ranking_uses_frequency_and_recency() {
        let candidates = vec![
            MentionCandidate {
                handle: "alex".into(),
                name: "Alex".into(),
                nicknames: vec![],
            },
            MentionCandidate {
                handle: "alice".into(),
                name: "Alice".into(),
                nicknames: vec![],
            },
        ];
        let mut usage = std::collections::HashMap::new();
        usage.insert(
            "alex".to_string(),
            UsageStat {
                count: 1,
                last_used: 10,
            },
        );
        usage.insert(
            "alice".to_string(),
            UsageStat {
                count: 5,
                last_used: 5,
            },
        );
        let ranked = filter_mentions(&candidates, "al", &usage, 20);
        assert_eq!(ranked[0].handle.as_ref(), "alice"); // higher frequency wins among same match rank
        let mut usage2 = std::collections::HashMap::new();
        usage2.insert(
            "alex".to_string(),
            UsageStat {
                count: 2,
                last_used: 20,
            },
        );
        usage2.insert(
            "alice".to_string(),
            UsageStat {
                count: 2,
                last_used: 5,
            },
        );
        let ranked2 = filter_mentions(&candidates, "al", &usage2, 20);
        assert_eq!(ranked2[0].handle.as_ref(), "alex"); // more recent when counts equal
    }

    #[test]
    fn mention_usage_lookup_is_case_insensitive_for_handles() {
        let candidates = vec![
            MentionCandidate {
                handle: "CodeX".into(),
                name: "Codex".into(),
                nicknames: vec![],
            },
            MentionCandidate {
                handle: "Coder".into(),
                name: "Coder".into(),
                nicknames: vec![],
            },
        ];
        let mut usage = std::collections::HashMap::new();
        usage.insert(
            "codex".to_string(),
            UsageStat {
                count: 10,
                last_used: 50,
            },
        ); // stored normalized
        let ranked = filter_mentions(&candidates, "cod", &usage, 60);
        assert_eq!(ranked.first().map(|c| c.handle.as_ref()), Some("CodeX"));
    }

    #[test]
    fn suggestion_usage_persists_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!(
            "cc-composer-test-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("composer_suggestions.json");
        let mut mentions = std::collections::HashMap::new();
        mentions.insert(
            "codex".to_string(),
            UsageStat {
                count: 3,
                last_used: 42,
            },
        );
        let mut emoji = std::collections::HashMap::new();
        emoji.insert(
            "👍".to_string(),
            UsageStat {
                count: 2,
                last_used: 99,
            },
        );
        save_suggestion_usage(Some(&path), &mentions, &emoji);
        let (lm, le) = load_suggestion_usage(Some(&path));
        assert_eq!(
            lm.get("codex"),
            Some(&UsageStat {
                count: 3,
                last_used: 42
            })
        );
        assert_eq!(
            le.get("👍"),
            Some(&UsageStat {
                count: 2,
                last_used: 99
            })
        );
        // malformed JSON recovers to empty state
        std::fs::write(&path, "{not valid json").unwrap();
        let (lm2, le2) = load_suggestion_usage(Some(&path));
        assert!(lm2.is_empty() && le2.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggestion_usage_bounds_and_no_path() {
        let dir = std::env::temp_dir().join(format!(
            "cc-composer-bounds-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("composer_suggestions.json");
        // oversized count clamps
        let text =
            r#"{"version":1,"mentions":{"a":{"count":99999999999,"last_used":5}},"emoji":{}}"#;
        std::fs::write(&path, text).unwrap();
        let (lm, _) = load_suggestion_usage(Some(&path));
        assert_eq!(
            lm.get("a"),
            Some(&UsageStat {
                count: u32::MAX,
                last_used: 5
            })
        );
        // no path => no persistence side effects / empty load
        let mut mentions = std::collections::HashMap::new();
        mentions.insert(
            "x".to_string(),
            UsageStat {
                count: 1,
                last_used: 1,
            },
        );
        let emoji = std::collections::HashMap::new();
        save_suggestion_usage(None, &mentions, &emoji);
        let (lm3, le3) = load_suggestion_usage(None);
        assert!(lm3.is_empty() && le3.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emoji_mru_ranking_empty_query_and_stability() {
        let mk = |sc: &str, e: &str| EmojiMatch {
            shortcode: sc.into(),
            emoji: e.into(),
            name: sc.into(),
        };
        let mut matches = vec![mk("a", "😀"), mk("b", "😃"), mk("c", "😄")];
        let mut usage = std::collections::HashMap::new();
        usage.insert(
            "😃".to_string(),
            UsageStat {
                count: 5,
                last_used: 10,
            },
        );
        usage.insert(
            "😄".to_string(),
            UsageStat {
                count: 2,
                last_used: 20,
            },
        );
        rank_emoji_matches(&mut matches, &usage);
        assert_eq!(matches[0].emoji.as_ref(), "😃"); // highest count
        assert_eq!(matches[1].emoji.as_ref(), "😄");
        assert_eq!(matches[2].emoji.as_ref(), "😀");
        // stability when usage equal: preserve original order among unused
        let mut matches2 = vec![mk("a", "😀"), mk("b", "😃"), mk("c", "😄")];
        let usage2 = std::collections::HashMap::new();
        rank_emoji_matches(&mut matches2, &usage2);
        assert_eq!(
            matches2
                .iter()
                .map(|m| m.emoji.as_ref())
                .collect::<Vec<_>>(),
            vec!["😀", "😃", "😄"]
        );
        // typed query: used emoji still rise above unused matches
        let mut matches3 = vec![mk("a", "😀"), mk("b", "😃")];
        let mut usage3 = std::collections::HashMap::new();
        usage3.insert(
            "😃".to_string(),
            UsageStat {
                count: 99,
                last_used: 99,
            },
        );
        rank_emoji_matches(&mut matches3, &usage3);
        assert_eq!(
            matches3
                .iter()
                .map(|m| m.emoji.as_ref())
                .collect::<Vec<_>>(),
            vec!["😃", "😀"]
        );
    }

    #[test]
    fn emoji_query_parses_shortcode_token() {
        assert_eq!(emoji_query("hi :thu", 7), Some((3..7, "thu".to_string())));
        assert_eq!(emoji_query("time: 10:30", 10), None);
        assert_eq!(emoji_query("http://x", 7), None);
        assert_eq!(emoji_query(":fire:", 6), Some((0..6, "fire".to_string())));
        assert_eq!(emoji_query("(:sm", 4), Some((1..4, "sm".to_string())));
    }
}
