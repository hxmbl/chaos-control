//! Text-editing logic behind `TextInput` that needs no GPUI: word boundaries
//! for word-wise moves and deletes, and the undo history. Kept apart so it can
//! be unit-tested.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use unicode_segmentation::UnicodeSegmentation as _;

/// What a grapheme counts as when moving or deleting by word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Space,
    /// Letters, digits, `_` and anything joined to them (accents, emoji).
    Word,
    /// A run of punctuation counts as one word, so `foo.bar` stops at the dot.
    Punct,
}

fn class(grapheme: &str) -> Class {
    let c = grapheme.chars().next().unwrap_or(' ');
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' || (!c.is_ascii() && !is_unicode_punct(c)) {
        Class::Word
    } else {
        Class::Punct
    }
}

/// The common non-ASCII punctuation that should split words (quotes, dashes,
/// ellipsis). Everything else outside ASCII counts as part of a word.
fn is_unicode_punct(c: char) -> bool {
    matches!(
        c,
        '\u{2010}'..='\u{2027}' | '\u{2030}'..='\u{205E}' | '\u{3000}'..='\u{303F}' | '\u{FF01}'..='\u{FF0F}'
    )
}

/// Where alt-left (macOS) / ctrl-left (elsewhere) lands: skip spaces to the
/// left, then go to the start of the word (or punctuation run) before them.
pub fn previous_word_start(text: &str, offset: usize) -> usize {
    let mut graphemes = text[..offset.min(text.len())]
        .grapheme_indices(true)
        .rev()
        .peekable();
    while graphemes
        .next_if(|(_, g)| class(g) == Class::Space)
        .is_some()
    {}
    let Some(&(mut start, first)) = graphemes.peek() else {
        return 0;
    };
    let kind = class(first);
    while let Some((ix, _)) = graphemes.next_if(|(_, g)| class(g) == kind) {
        start = ix;
    }
    start
}

/// Where alt-right / ctrl-right lands: skip spaces to the right, then go to the
/// end of the word (or punctuation run) after them. This is the macOS and GTK
/// behaviour; Windows stops at the next word's start instead.
pub fn next_word_end(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let mut graphemes = text[offset..].grapheme_indices(true).peekable();
    while graphemes
        .next_if(|(_, g)| class(g) == Class::Space)
        .is_some()
    {}
    let Some(&(_, first)) = graphemes.peek() else {
        return text.len();
    };
    let kind = class(first);
    let mut end = text.len() - offset;
    while let Some((ix, g)) = graphemes.next_if(|(_, g)| class(g) == kind) {
        end = ix + g.len();
    }
    offset + end
}

/// Grapheme-aware single steps.
pub fn previous_grapheme(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .rev()
        .find_map(|(ix, _)| (ix < offset).then_some(ix))
        .unwrap_or(0)
}

pub fn next_grapheme(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .find_map(|(ix, _)| (ix > offset).then_some(ix))
        .unwrap_or(text.len())
}

/// The word (or space or punctuation run) under `offset`, for double-click.
pub fn word_range_at(text: &str, offset: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let graphemes: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    // The grapheme under the pointer; at the very end, the last one.
    let at = graphemes
        .iter()
        .rposition(|(ix, _)| *ix <= offset)
        .unwrap_or(0)
        .min(graphemes.len() - 1);
    let kind = class(graphemes[at].1);
    let mut start = at;
    while start > 0 && class(graphemes[start - 1].1) == kind {
        start -= 1;
    }
    let mut end = at + 1;
    while end < graphemes.len() && class(graphemes[end].1) == kind {
        end += 1;
    }
    let end_ix = graphemes.get(end).map_or(text.len(), |(ix, _)| *ix);
    graphemes[start].0..end_ix
}

/// The text and selection at one moment, as the undo history stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub text: String,
    pub selection: Range<usize>,
    pub reversed: bool,
}

/// What an edit was, so consecutive typing undoes as one step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// Typing (including IME composition).
    Insert,
    /// Backspace or delete one grapheme at a time.
    Delete,
    /// Anything that should always be its own undo step: paste, cut, word
    /// and line deletes.
    Other,
}

/// Typing pauses longer than this start a new undo step.
const COALESCE: Duration = Duration::from_millis(1000);
const MAX_STEPS: usize = 200;

/// Undo and redo for one field. Each step stores the state before the edit.
#[derive(Default)]
pub struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The last edit, if the next one may join its step.
    open_group: Option<(EditKind, Instant)>,
}

impl History {
    /// Call before applying an edit, with the state it is about to replace.
    pub fn record(&mut self, before: Snapshot, kind: EditKind, now: Instant) {
        self.redo.clear();
        let joins = kind != EditKind::Other
            && self.open_group.is_some_and(|(last, at)| {
                last == kind && now.saturating_duration_since(at) < COALESCE
            });
        if !joins {
            self.undo.push(before);
            if self.undo.len() > MAX_STEPS {
                self.undo.remove(0);
            }
        }
        self.open_group = (kind != EditKind::Other).then_some((kind, now));
    }

    /// The cursor moved or the field lost focus: the next edit is a new step.
    pub fn break_group(&mut self) {
        self.open_group = None;
    }

    pub fn undo(&mut self, current: Snapshot) -> Option<Snapshot> {
        let previous = self.undo.pop()?;
        self.redo.push(current);
        self.open_group = None;
        Some(previous)
    }

    pub fn redo(&mut self, current: Snapshot) -> Option<Snapshot> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        self.open_group = None;
        Some(next)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_left_skips_spaces_then_the_word() {
        let s = "hello brave  world";
        assert_eq!(previous_word_start(s, s.len()), 13);
        assert_eq!(previous_word_start(s, 13), 6);
        assert_eq!(previous_word_start(s, 8), 6);
        assert_eq!(previous_word_start(s, 6), 0);
        assert_eq!(previous_word_start(s, 0), 0);
        assert_eq!(previous_word_start("   ", 3), 0);
    }

    #[test]
    fn word_right_skips_spaces_then_the_word() {
        let s = "hello brave  world";
        assert_eq!(next_word_end(s, 0), 5);
        assert_eq!(next_word_end(s, 5), 11);
        assert_eq!(next_word_end(s, 11), s.len());
        assert_eq!(next_word_end(s, s.len()), s.len());
        assert_eq!(next_word_end("a   ", 1), 4);
    }

    #[test]
    fn punctuation_runs_are_their_own_word() {
        let s = "see foo.bar(); ok";
        assert_eq!(previous_word_start(s, 11), 8); // "bar"
        assert_eq!(previous_word_start(s, 8), 7); // "."
        assert_eq!(next_word_end(s, 11), 14); // "();"
        assert_eq!(next_word_end("snake_case x", 0), 10);
    }

    #[test]
    fn words_are_unicode_aware() {
        let s = "café naïve 日本語 👍🏽 done";
        let naive = s.find("naïve").unwrap();
        assert_eq!(previous_word_start(s, naive + "naïve".len()), naive);
        assert_eq!(next_word_end(s, 0), "café".len());
        // An emoji with a skin-tone modifier moves as one piece.
        let thumb = s.find('👍').unwrap();
        assert_eq!(next_grapheme(s, thumb), thumb + "👍🏽".len());
        assert_eq!(previous_grapheme(s, thumb + "👍🏽".len()), thumb);
        // Combining accent stays with its letter.
        let e = "e\u{301}x";
        assert_eq!(next_grapheme(e, 0), 3);
    }

    #[test]
    fn double_click_selects_the_word_under_the_pointer() {
        let s = "hello brave world";
        assert_eq!(word_range_at(s, 7), 6..11);
        assert_eq!(word_range_at(s, 6), 6..11);
        assert_eq!(word_range_at(s, 5), 5..6); // the space
        assert_eq!(word_range_at(s, s.len()), 12..17);
        assert_eq!(word_range_at("", 0), 0..0);
    }

    fn snap(text: &str) -> Snapshot {
        Snapshot {
            text: text.into(),
            selection: text.len()..text.len(),
            reversed: false,
        }
    }

    #[test]
    fn typing_undoes_as_one_step_until_a_pause_or_a_move() {
        let t0 = Instant::now();
        let mut h = History::default();
        h.record(snap(""), EditKind::Insert, t0);
        h.record(snap("h"), EditKind::Insert, t0 + Duration::from_millis(100));
        h.record(
            snap("hi"),
            EditKind::Insert,
            t0 + Duration::from_millis(200),
        );
        // A pause starts a new step.
        h.record(snap("hi!"), EditKind::Insert, t0 + Duration::from_secs(3));
        assert_eq!(h.undo(snap("hi!?")).unwrap().text, "hi!");
        assert_eq!(h.undo(snap("hi!")).unwrap().text, "");
        assert!(h.undo(snap("")).is_none());

        let mut h = History::default();
        h.record(snap(""), EditKind::Insert, t0);
        h.break_group();
        h.record(snap("a"), EditKind::Insert, t0);
        assert_eq!(h.undo(snap("ab")).unwrap().text, "a");
    }

    #[test]
    fn switching_between_typing_and_deleting_starts_a_new_step() {
        let t0 = Instant::now();
        let mut h = History::default();
        h.record(snap(""), EditKind::Insert, t0);
        h.record(snap("abc"), EditKind::Delete, t0);
        h.record(snap("ab"), EditKind::Delete, t0);
        assert_eq!(h.undo(snap("a")).unwrap().text, "abc");
        assert_eq!(h.undo(snap("abc")).unwrap().text, "");
    }

    #[test]
    fn paste_is_always_its_own_step_and_redo_replays() {
        let t0 = Instant::now();
        let mut h = History::default();
        h.record(snap(""), EditKind::Other, t0);
        h.record(snap("x"), EditKind::Other, t0);
        let back = h.undo(snap("xy")).unwrap();
        assert_eq!(back.text, "x");
        assert_eq!(h.redo(back).unwrap().text, "xy");
        // A new edit after undo drops the redo stack.
        let back = h.undo(snap("xy")).unwrap();
        h.record(back, EditKind::Insert, t0);
        assert!(h.redo(snap("xz")).is_none());
    }
}
