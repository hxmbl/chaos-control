//! Text field: single-line by default, or multi-line (the composer) with
//! [`TextInput::multiline`].
//!
//! Adapted from GPUI's `examples/input.rs` (Apache-2.0, Copyright Zed Industries):
//! platform IME via `EntityInputHandler`, grapheme-aware cursor movement, mouse
//! selection and clipboard. Added here: placeholder and theme colours, optional
//! centring, a horizontal scroll that keeps the cursor visible, a caret that
//! animates between offsets, and events. A multi-line field wraps its text,
//! grows with it up to a maximum number of rows and then scrolls vertically.

use std::ops::Range;

use std::time::Instant;

use gpui::{
    App, AvailableSpace, Bounds, ClipboardItem, Context, CursorStyle, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, Hsla, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, PathBuilder, Pixels, Point, ScrollWheelEvent, ShapedLine,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine,
    actions, div, fill, point, prelude::*, px, relative, size,
};

use crate::{
    ActiveTheme,
    cursor_animation::{CursorAnimation, CursorViewport},
    editing::{self, EditKind, History, Snapshot},
};

/// The bar is thin enough to read as a caret and wide enough to stretch.
const CARET_WIDTH: f32 = 1.5;
/// How far a selection reaches past the end of a line when it includes the
/// line break, so a selected empty line still shows.
const BREAK_SELECTION_WIDTH: f32 = 4.;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToStart,
        DeleteToEnd,
        Left,
        Right,
        WordLeft,
        WordRight,
        SelectLeft,
        SelectRight,
        SelectWordLeft,
        SelectWordRight,
        SelectAll,
        Home,
        End,
        SelectToStart,
        SelectToEnd,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        Undo,
        Redo,
        Submit,
        Newline,
        Up,
        Down,
        SelectUp,
        SelectDown,
    ]
);

const CONTEXT: &str = "TextInput";
/// The key context a multi-line field adds, so line keys only apply there.
pub const MULTILINE_KEY_CONTEXT: &str = "TextInput multiline";
const MULTILINE: &str = "TextInput && multiline";

/// The editing keys people expect in any text field on their OS. `secondary`
/// is cmd on macOS and ctrl elsewhere. "Start/end" is the start/end of the
/// text. In a single-line field up/down are left unbound so the search field
/// can use them for its results; a multi-line field (the composer) uses them to
/// move between lines, and takes shift-enter for a line break while enter
/// still submits, as in Slack and Discord.
pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys(bindings());
}

/// Every text field binding, for `bind_keys` and for keymap tests (here and
/// in views that wrap a field, like the composer).
pub fn bindings() -> Vec<KeyBinding> {
    let ctx = Some(CONTEXT);
    let multiline = Some(MULTILINE);
    // Everywhere.
    let mut bindings = vec![
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("shift-backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("shift-home", SelectToStart, ctx),
        KeyBinding::new("shift-end", SelectToEnd, ctx),
        KeyBinding::new("enter", Submit, ctx),
        KeyBinding::new("secondary-a", SelectAll, ctx),
        KeyBinding::new("secondary-v", Paste, ctx),
        KeyBinding::new("secondary-c", Copy, ctx),
        KeyBinding::new("secondary-x", Cut, ctx),
        KeyBinding::new("secondary-z", Undo, ctx),
        KeyBinding::new("secondary-shift-z", Redo, ctx),
        KeyBinding::new("shift-enter", Newline, multiline),
        KeyBinding::new("up", Up, multiline),
        KeyBinding::new("down", Down, multiline),
        KeyBinding::new("shift-up", SelectUp, multiline),
        KeyBinding::new("shift-down", SelectDown, multiline),
    ];
    if cfg!(target_os = "macos") {
        bindings.extend([
            KeyBinding::new("alt-left", WordLeft, ctx),
            KeyBinding::new("alt-right", WordRight, ctx),
            KeyBinding::new("alt-shift-left", SelectWordLeft, ctx),
            KeyBinding::new("alt-shift-right", SelectWordRight, ctx),
            KeyBinding::new("cmd-left", Home, ctx),
            KeyBinding::new("cmd-right", End, ctx),
            KeyBinding::new("cmd-up", Home, ctx),
            KeyBinding::new("cmd-down", End, ctx),
            KeyBinding::new("cmd-shift-left", SelectToStart, ctx),
            KeyBinding::new("cmd-shift-right", SelectToEnd, ctx),
            KeyBinding::new("cmd-shift-up", SelectToStart, ctx),
            KeyBinding::new("cmd-shift-down", SelectToEnd, ctx),
            KeyBinding::new("alt-backspace", DeleteWordLeft, ctx),
            KeyBinding::new("alt-delete", DeleteWordRight, ctx),
            KeyBinding::new("cmd-backspace", DeleteToStart, ctx),
            KeyBinding::new("cmd-delete", DeleteToEnd, ctx),
            // Cocoa's Emacs-style keys, which every native macOS field has.
            KeyBinding::new("ctrl-a", Home, ctx),
            KeyBinding::new("ctrl-e", End, ctx),
            KeyBinding::new("ctrl-b", Left, ctx),
            KeyBinding::new("ctrl-f", Right, ctx),
            KeyBinding::new("ctrl-h", Backspace, ctx),
            KeyBinding::new("ctrl-d", Delete, ctx),
            KeyBinding::new("ctrl-k", DeleteToEnd, ctx),
            KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, ctx),
        ]);
    } else {
        bindings.extend([
            KeyBinding::new("ctrl-left", WordLeft, ctx),
            KeyBinding::new("ctrl-right", WordRight, ctx),
            KeyBinding::new("ctrl-shift-left", SelectWordLeft, ctx),
            KeyBinding::new("ctrl-shift-right", SelectWordRight, ctx),
            KeyBinding::new("ctrl-home", Home, ctx),
            KeyBinding::new("ctrl-end", End, ctx),
            KeyBinding::new("ctrl-shift-home", SelectToStart, ctx),
            KeyBinding::new("ctrl-shift-end", SelectToEnd, ctx),
            KeyBinding::new("ctrl-backspace", DeleteWordLeft, ctx),
            KeyBinding::new("ctrl-delete", DeleteWordRight, ctx),
            KeyBinding::new("ctrl-y", Redo, ctx),
            // The older CUA clipboard keys, still standard on Windows and Linux.
            KeyBinding::new("shift-delete", Cut, ctx),
            KeyBinding::new("ctrl-insert", Copy, ctx),
            KeyBinding::new("shift-insert", Paste, ctx),
        ]);
    }
    bindings
}

pub enum TextInputEvent {
    Changed,
    Submit,
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    centered: bool,
    /// Holds line breaks and grows to `max_lines` rows (see [`Self::multiline`]).
    multiline: bool,
    max_lines: usize,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    /// A multi-line field's shaped text from the last paint (instead of
    /// `last_layout`).
    last_multi: Option<MultiLayout>,
    last_bounds: Option<Bounds<Pixels>>,
    /// Where the shaped line started on the last paint (centring and scrolling).
    last_origin_x: Pixels,
    is_selecting: bool,
    history: History,
    cursor_animation: CursorAnimation,
    /// A multi-line field's vertical scroll, from the top of its text.
    scroll_y: Pixels,
    /// Set by edits and cursor moves: the next paint scrolls the caret into
    /// view. Left unset by the mouse wheel, so reading back stays put.
    autoscroll: bool,
    /// The column up/down aim for, kept across a run of up/down presses so
    /// passing a short line doesn't lose it.
    goal_x: Option<Pixels>,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            centered: false,
            multiline: false,
            max_lines: 1,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_multi: None,
            last_bounds: None,
            last_origin_x: px(0.),
            is_selecting: false,
            history: History::default(),
            cursor_animation: CursorAnimation::default(),
            scroll_y: px(0.),
            autoscroll: false,
            goal_x: None,
        }
    }

    /// Makes this a multi-line field, as the composer is: shift-enter inserts
    /// a line break (enter still submits), pasted text keeps its line breaks,
    /// up/down move between lines, and the field grows with its text up to
    /// `max_lines` rows, then scrolls.
    pub fn multiline(mut self, max_lines: usize) -> Self {
        self.multiline = true;
        self.max_lines = max_lines.max(1);
        self
    }

    /// Centre the text (and placeholder) horizontally, as in the titlebar search.
    pub fn centered(mut self) -> Self {
        self.centered = true;
        self
    }

    /// Switch centring on or off after construction (the search field is
    /// centred in the titlebar pill and left-aligned once it opens).
    pub fn set_centered(&mut self, centered: bool, cx: &mut Context<Self>) {
        if self.centered != centered {
            self.centered = centered;
            cx.notify();
        }
    }

    /// Selects everything, as cmd/ctrl-K does when search is already open.
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    pub fn text(&self) -> &SharedString {
        &self.content
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let placeholder = placeholder.into();
        if placeholder != self.placeholder {
            self.placeholder = placeholder;
            cx.notify();
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.content = SharedString::default();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        // A cleared field (a sent message, an emptied search) starts fresh.
        self.history.clear();
        self.scroll_y = px(0.);
        self.goal_x = None;
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(
                editing::previous_grapheme(&self.content, self.cursor_offset()),
                cx,
            );
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(
                editing::next_grapheme(&self.content, self.selected_range.end),
                cx,
            );
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(
            editing::previous_word_start(&self.content, self.cursor_offset()),
            cx,
        );
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(
            editing::next_word_end(&self.content, self.cursor_offset()),
            cx,
        );
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(
            editing::previous_grapheme(&self.content, self.cursor_offset()),
            cx,
        );
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(
            editing::next_grapheme(&self.content, self.cursor_offset()),
            cx,
        );
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(
            editing::previous_word_start(&self.content, self.cursor_offset()),
            cx,
        );
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(
            editing::next_word_end(&self.content, self.cursor_offset()),
            cx,
        );
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    /// Enter. While an IME composition is open (marked text, e.g. pinyin or
    /// kana being converted), Enter confirms it and does not submit.
    ///
    /// On macOS GPUI hands keys to the IME first while there is marked text
    /// (`marked_text_range`), and Linux IMEs grab the keyboard while
    /// pre-editing, so the IME normally commits and Enter never gets here.
    /// This guards the case where it does anyway: the composed text is kept
    /// as typed and the message waits for the next Enter.
    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        match enter(self.marked_range.is_some()) {
            Enter::ConfirmComposition => {
                self.marked_range = None;
                cx.notify();
            }
            Enter::Submit => cx.emit(TextInputEvent::Submit),
        }
    }

    /// Shift-enter in a multi-line field. Single-line fields never get it (the
    /// binding needs the multiline context), but pass it on if they do.
    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if !self.multiline {
            cx.propagate();
            return;
        }
        let range = self
            .marked_range
            .clone()
            .unwrap_or(self.selected_range.clone());
        self.edit(range, "\n", EditKind::Insert, cx);
    }

    /// Where up (or down) lands from the cursor, aiming for `goal_x`: the same
    /// column a row above (below), the start of the text from the first row,
    /// and the end from the last, as text editors do.
    fn vertical_target(&self, down: bool, goal_x: Pixels) -> Option<usize> {
        if self.content.is_empty() {
            return None;
        }
        let multi = self.last_multi.as_ref()?;
        let lh = multi.line_height;
        let caret = multi.position_for_offset(self.cursor_offset());
        let row = (caret.y / lh).round() as usize;
        Some(if down {
            if row + 1 >= multi.rows {
                self.content.len()
            } else {
                multi.offset_for_position(point(goal_x, caret.y + lh * 1.5))
            }
        } else if row == 0 {
            0
        } else {
            multi.offset_for_position(point(goal_x, caret.y - lh * 0.5))
        })
    }

    /// Up/down and shift-up/down. Returns `false` (and moves nothing) when the
    /// field has no lines to move through.
    fn move_vertically(&mut self, down: bool, select: bool, cx: &mut Context<Self>) -> bool {
        if !self.multiline {
            return false;
        }
        let Some(multi) = self.last_multi.as_ref() else {
            return false;
        };
        let goal_x = self
            .goal_x
            .unwrap_or_else(|| multi.position_for_offset(self.cursor_offset()).x);
        // Nothing to move through (an empty field): pass the key on, so the
        // composer can give up in an empty field a meaning of its own later.
        let Some(target) = self.vertical_target(down, goal_x) else {
            return false;
        };
        if select {
            self.select_to(target, cx);
        } else {
            self.move_to(target, cx);
        }
        self.goal_x = Some(goal_x);
        true
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if !self.move_vertically(false, false, cx) {
            cx.propagate();
        }
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if !self.move_vertically(true, false, cx) {
            cx.propagate();
        }
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        if !self.move_vertically(false, true, cx) {
            cx.propagate();
        }
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        if !self.move_vertically(true, true, cx) {
            cx.propagate();
        }
    }

    /// The mouse wheel over a multi-line field that has more rows than it
    /// shows. Clamped on the next paint.
    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((line_height, rows)) = self.last_multi.as_ref().map(|m| (m.line_height, m.rows))
        else {
            return;
        };
        let Some(bounds) = self.last_bounds else {
            return;
        };
        let content = line_height * rows as f32;
        if content <= bounds.size.height {
            return;
        }
        let delta = event.delta.pixel_delta(line_height);
        self.scroll_y = px(editing::clamp_scroll(
            f32::from(self.scroll_y - delta.y),
            bounds.size.height.into(),
            content.into(),
        ));
        cx.stop_propagation();
        cx.notify();
    }

    /// Deletes the selection, or when there is none, from the cursor to
    /// `target` (either side of it).
    fn delete_toward(&mut self, target: usize, kind: EditKind, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            cursor.min(target)..cursor.max(target)
        } else {
            self.selected_range.clone()
        };
        if !range.is_empty() {
            self.edit(range, "", kind, cx);
        }
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let target = editing::previous_grapheme(&self.content, self.cursor_offset());
        self.delete_toward(target, EditKind::Delete, cx)
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let target = editing::next_grapheme(&self.content, self.cursor_offset());
        self.delete_toward(target, EditKind::Delete, cx)
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = editing::previous_word_start(&self.content, self.cursor_offset());
        self.delete_toward(target, EditKind::Other, cx)
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = editing::next_word_end(&self.content, self.cursor_offset());
        self.delete_toward(target, EditKind::Other, cx)
    }

    fn delete_to_start(&mut self, _: &DeleteToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_toward(0, EditKind::Other, cx)
    }

    fn delete_to_end(&mut self, _: &DeleteToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_toward(self.content.len(), EditKind::Other, cx)
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.content.to_string(),
            selection: self.selected_range.clone(),
            reversed: self.selection_reversed,
        }
    }

    fn restore(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        self.content = snapshot.text.into();
        self.selected_range = snapshot.selection;
        self.selection_reversed = snapshot.reversed;
        self.marked_range = None;
        self.autoscroll = true;
        self.goal_x = None;
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.history.undo(self.snapshot()) {
            self.restore(previous, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.history.redo(self.snapshot()) {
            self.restore(next, cx);
        }
    }

    /// Every change to the text goes through here, so it lands in the undo
    /// history. Leaves the cursor after the new text.
    fn edit(
        &mut self,
        range: Range<usize>,
        new_text: &str,
        kind: EditKind,
        cx: &mut Context<Self>,
    ) {
        self.history.record(self.snapshot(), kind, Instant::now());
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        self.autoscroll = true;
        self.goal_x = None;
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        let index = self.index_for_mouse_position(event.position);
        match event.click_count {
            // Double click selects the word under the pointer, triple the line.
            2 => {
                let word = editing::word_range_at(&self.content, index);
                self.move_to(word.start, cx);
                self.select_to(word.end, cx);
            }
            n if n >= 3 => {
                self.move_to(0, cx);
                self.select_to(self.content.len(), cx);
            }
            _ => {
                self.is_selecting = true;
                if event.modifiers.shift {
                    self.select_to(index, cx);
                } else {
                    self.move_to(index, cx)
                }
            }
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let range = self
                .marked_range
                .clone()
                .unwrap_or(self.selected_range.clone());
            // A multi-line field keeps the pasted line breaks.
            let text = editing::normalize_breaks(&text, self.multiline);
            self.edit(range, &text, EditKind::Other, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.edit(self.selected_range.clone(), "", EditKind::Other, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.history.break_group();
        self.autoscroll = true;
        self.goal_x = None;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        if self.multiline {
            let (Some(bounds), Some(multi)) = (self.last_bounds, self.last_multi.as_ref()) else {
                return 0;
            };
            return multi.offset_for_position(self.multi_local(position, bounds));
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - self.last_origin_x)
    }

    /// Converts a window position to a point in the multi-line text (its top
    /// left is the origin, scrolling included).
    fn multi_local(&self, position: Point<Pixels>, bounds: Bounds<Pixels>) -> Point<Pixels> {
        point(
            position.x - bounds.left(),
            position.y - bounds.top() + self.scroll_y,
        )
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.history.break_group();
        self.autoscroll = true;
        self.goal_x = None;
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        // A return that reaches the field as text (some IMEs) is a line break in
        // a multi-line field and a space in a single-line one.
        let new_text = editing::normalize_breaks(new_text, self.multiline);
        self.edit(range, &new_text, EditKind::Insert, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let new_text = editing::normalize_breaks(new_text, self.multiline);
        let new_text = new_text.as_ref();

        self.history
            .record(self.snapshot(), EditKind::Insert, Instant::now());
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.marked_range =
            (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.autoscroll = true;
        self.goal_x = None;
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if self.multiline {
            let multi = self.last_multi.as_ref()?;
            let range = self.range_from_utf16(&range_utf16);
            let start = multi.position_for_offset(range.start);
            let end = multi.position_for_offset(range.end);
            let right = if end.y == start.y { end.x } else { start.x };
            let top = bounds.top() + start.y - self.scroll_y;
            return Some(Bounds::from_corners(
                point(bounds.left() + start.x, top),
                point(bounds.left() + right.max(start.x), top + multi.line_height),
            ));
        }
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let x0 = self.last_origin_x - bounds.left();
        Some(Bounds::from_corners(
            point(
                bounds.left() + x0 + last_layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + x0 + last_layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        bounds.localize(&point)?;
        if self.multiline {
            let multi = self.last_multi.as_ref()?;
            let offset = multi.offset_for_position(self.multi_local(point, bounds));
            return Some(self.offset_to_utf16(offset));
        }
        let last_layout = self.last_layout.as_ref()?;
        let utf8_index = last_layout.index_for_x(point.x - self.last_origin_x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    origin_x: Pixels,
    cursor: Option<CursorPaint>,
    selection: Option<PaintQuad>,
    /// Set instead of `line` and `selection` in a multi-line field.
    multi: Option<MultiPaint>,
}

/// What a multi-line field paints this frame.
struct MultiPaint {
    layout: MultiLayout,
    selections: Vec<PaintQuad>,
    /// Where the text's top-left corner is drawn (the scroll moves it up).
    origin: Point<Pixels>,
}

/// The width a multi-line field wraps at: its own, less room for the caret.
fn wrap_width(width: Pixels) -> Pixels {
    (width - px(CARET_WIDTH)).max(px(1.))
}

/// What Enter does in a text field.
#[derive(Debug, PartialEq, Eq)]
enum Enter {
    /// An IME composition is open: commit it, don't submit.
    ConfirmComposition,
    Submit,
}

fn enter(composing: bool) -> Enter {
    if composing {
        Enter::ConfirmComposition
    } else {
        Enter::Submit
    }
}

/// The style runs for `run`'s text, with the IME's marked range underlined.
fn text_runs(run: TextRun, marked: Option<&Range<usize>>) -> Vec<TextRun> {
    let Some(marked) = marked else {
        return vec![run];
    };
    let len = run.len;
    [
        TextRun {
            len: marked.start,
            ..run.clone()
        },
        TextRun {
            len: marked.end - marked.start,
            underline: Some(UnderlineStyle {
                color: Some(run.color),
                thickness: px(1.0),
                wavy: false,
            }),
            ..run.clone()
        },
        TextRun {
            len: len - marked.end,
            ..run
        },
    ]
    .into_iter()
    .filter(|run| run.len > 0)
    .collect()
}

/// A multi-line field's text, shaped and wrapped: one `WrappedLine` per
/// `\n`-separated line, each of which may wrap onto several rows.
struct MultiLayout {
    lines: Vec<WrappedLine>,
    /// Byte offset of each line in the text.
    starts: Vec<usize>,
    /// The row each line starts on.
    first_rows: Vec<usize>,
    /// Rows in the whole text, at least one.
    rows: usize,
    line_height: Pixels,
}

impl MultiLayout {
    fn shape(
        text: SharedString,
        runs: &[TextRun],
        font_size: Pixels,
        wrap_width: Option<Pixels>,
        line_height: Pixels,
        window: &Window,
    ) -> Self {
        let lines = window
            .text_system()
            .shape_text(text, font_size, runs, wrap_width, None)
            .map(|lines| lines.into_vec())
            .unwrap_or_default();
        let mut starts = Vec::with_capacity(lines.len());
        let mut first_rows = Vec::with_capacity(lines.len());
        let (mut start, mut row) = (0, 0);
        for line in &lines {
            starts.push(start);
            first_rows.push(row);
            start += line.len() + 1;
            row += line.wrap_boundaries().len() + 1;
        }
        Self {
            lines,
            starts,
            first_rows,
            rows: row.max(1),
            line_height,
        }
    }

    /// Byte offsets (within `line`) where each of its rows starts.
    fn row_starts(line: &WrappedLine) -> Vec<usize> {
        std::iter::once(0)
            .chain(line.wrap_boundaries().iter().map(|boundary| {
                line.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].index
            }))
            .collect()
    }

    fn line_at(&self, offset: usize) -> usize {
        self.starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1)
    }

    /// Where the caret for `offset` goes, from the text's top-left corner.
    fn position_for_offset(&self, offset: usize) -> Point<Pixels> {
        let ix = self.line_at(offset);
        let Some(line) = self.lines.get(ix) else {
            return point(px(0.), px(0.));
        };
        let local = offset.saturating_sub(self.starts[ix]).min(line.len());
        let at = line
            .position_for_index(local, self.line_height)
            .unwrap_or_else(|| point(line.width(), px(0.)));
        point(at.x, at.y + self.line_height * self.first_rows[ix] as f32)
    }

    /// The offset closest to `position` (from the text's top-left corner).
    /// Above the text is the first row, below it the last.
    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let row = if position.y < px(0.) {
            0
        } else {
            ((position.y / self.line_height) as usize).min(self.rows - 1)
        };
        let ix = self
            .first_rows
            .partition_point(|&first| first <= row)
            .saturating_sub(1);
        let line = &self.lines[ix];
        let local_row = row - self.first_rows[ix];
        let local = point(
            position.x.max(px(0.)),
            self.line_height * (local_row as f32 + 0.5),
        );
        let (Ok(index) | Err(index)) = line.closest_index_for_position(local, self.line_height);
        self.starts[ix] + index.min(line.len())
    }

    /// One highlight per row the selection touches. A selection that runs
    /// through a line break reaches a little past the line's end.
    fn selection_quads(
        &self,
        range: &Range<usize>,
        origin: Point<Pixels>,
        color: Hsla,
    ) -> Vec<PaintQuad> {
        let mut quads = Vec::new();
        for (ix, line) in self.lines.iter().enumerate() {
            let start = self.starts[ix];
            let end = start + line.len();
            if range.end < start || range.start > end {
                continue;
            }
            let rows = Self::row_starts(line);
            for (row, &row_start) in rows.iter().enumerate() {
                let row_end = rows.get(row + 1).copied().unwrap_or(line.len());
                let from = range.start.max(start + row_start);
                let to = range.end.min(start + row_end);
                let through_break = row + 1 == rows.len() && range.end > end;
                if from > to || (from == to && !through_break) {
                    continue;
                }
                let layout = &line.unwrapped_layout;
                let base = layout.x_for_index(row_start);
                let x0 = layout.x_for_index(from - start) - base;
                let mut x1 = layout.x_for_index(to - start) - base;
                if through_break {
                    x1 += px(BREAK_SELECTION_WIDTH);
                }
                let top = origin.y + self.line_height * (self.first_rows[ix] + row) as f32;
                quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x + x0, top),
                        point(origin.x + x1, top + self.line_height),
                    ),
                    color,
                ));
            }
        }
        quads
    }
}

/// Paints the caret: the trailing quad while it animates, else the bar.
fn paint_cursor(cursor: CursorPaint, window: &mut Window) {
    match cursor.corners {
        Some(corners) => {
            let mut builder = PathBuilder::fill();
            builder.add_polygon(&corners, true);
            match builder.build() {
                Ok(path) => window.paint_path(path, cursor.color.alpha(0.5)),
                Err(_) => window.paint_quad(cursor.target),
            }
        }
        None => window.paint_quad(cursor.target),
    }
}

impl TextElement {
    fn prepaint_multiline(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let theme = *cx.theme();
        let input = self.input.read(cx);
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();
        let line_height = window.line_height();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let empty = input.content.is_empty();
        let (display_text, color) = if empty {
            (input.placeholder.clone(), theme.muted)
        } else {
            (input.content.clone(), style.color)
        };
        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = text_runs(run, input.marked_range.as_ref().filter(|_| !empty));
        // The placeholder stays on one row; the text wraps to the field.
        let wrap = (!empty).then(|| wrap_width(bounds.size.width));
        let layout = MultiLayout::shape(display_text, &runs, font_size, wrap, line_height, window);

        let caret_at = if empty {
            point(px(0.), px(0.))
        } else {
            layout.position_for_offset(cursor)
        };
        let content_height = line_height * layout.rows as f32;
        let scroll_y = self.input.update(cx, |input, _| {
            let scroll = if std::mem::take(&mut input.autoscroll) {
                editing::scroll_to_reveal(
                    input.scroll_y.into(),
                    caret_at.y.into(),
                    line_height.into(),
                    bounds.size.height.into(),
                    content_height.into(),
                )
            } else {
                editing::clamp_scroll(
                    input.scroll_y.into(),
                    bounds.size.height.into(),
                    content_height.into(),
                )
            };
            input.scroll_y = px(scroll);
            input.scroll_y
        });
        let origin = point(bounds.left(), bounds.top() - scroll_y);
        let caret = Bounds::new(
            point(origin.x + caret_at.x, origin.y + caret_at.y),
            size(px(CARET_WIDTH), line_height),
        );

        let (selections, cursor) = if selected_range.is_empty() {
            let corners = self.input.update(cx, |input, _| {
                if !input.focus_handle.is_focused(window) {
                    input.cursor_animation.reset();
                    return None;
                }
                input.cursor_animation.update(
                    input.cursor_offset(),
                    caret,
                    CursorViewport::new(bounds.left(), bounds, line_height),
                    Instant::now(),
                )
            });
            if corners.is_some() {
                window.request_animation_frame();
            }
            (
                Vec::new(),
                Some(CursorPaint {
                    target: fill(caret, theme.accent),
                    color: theme.accent,
                    corners,
                }),
            )
        } else {
            self.input
                .update(cx, |input, _| input.cursor_animation.reset());
            (
                layout.selection_quads(&selected_range, origin, theme.selection),
                None,
            )
        };
        PrepaintState {
            line: None,
            origin_x: bounds.left(),
            cursor,
            selection: None,
            multi: Some(MultiPaint {
                layout,
                selections,
                origin,
            }),
        }
    }

    fn paint_multiline(
        &mut self,
        bounds: Bounds<Pixels>,
        paint: MultiPaint,
        cursor: Option<CursorPaint>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let MultiPaint {
            layout,
            selections,
            origin,
        } = paint;
        let line_height = layout.line_height;
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for selection in selections {
                window.paint_quad(selection);
            }
            for (ix, line) in layout.lines.iter().enumerate() {
                let top = origin.y + line_height * layout.first_rows[ix] as f32;
                let height = line_height * (line.wrap_boundaries().len() + 1) as f32;
                // Lines scrolled out of view are not drawn.
                if top > bounds.bottom() || top + height < bounds.top() {
                    continue;
                }
                line.paint(
                    point(origin.x, top),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .ok();
            }
            if focus_handle.is_focused(window)
                && let Some(cursor) = cursor
            {
                paint_cursor(cursor, window);
            }
        });
        self.input.update(cx, |input, _| {
            input.last_multi = Some(layout);
            input.last_bounds = Some(bounds);
        });
    }
}

/// The caret for this frame. `corners` is set while it is animating: painting
/// the quad between them gives the bar its trail.
struct CursorPaint {
    target: PaintQuad,
    color: Hsla,
    corners: Option<[Point<Pixels>; 4]>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let input = self.input.read(cx);
        if !input.multiline {
            style.size.height = window.line_height().into();
            return (window.request_layout(style, [], cx), ());
        }
        // A multi-line field is as tall as its wrapped text, up to max_lines
        // rows, which needs the width it is given: measure it then.
        let max_lines = input.max_lines;
        let content = input.content.clone();
        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let run = TextRun {
            len: content.len(),
            font: text_style.font(),
            color: text_style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, _| {
                let width = known.width.or(match available.width {
                    AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                let rows = if content.is_empty() {
                    1
                } else {
                    MultiLayout::shape(
                        content.clone(),
                        std::slice::from_ref(&run),
                        font_size,
                        width.map(wrap_width),
                        line_height,
                        window,
                    )
                    .rows
                };
                size(
                    width.unwrap_or(px(0.)),
                    line_height * editing::visible_rows(rows, max_lines) as f32,
                )
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if self.input.read(cx).multiline {
            return self.prepaint_multiline(bounds, window, cx);
        }
        let theme = *cx.theme();
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), theme.muted)
        } else {
            (content, style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = input.marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let mut runs = runs;
        let mut line =
            window
                .text_system()
                .shape_line(display_text.clone(), font_size, &runs, None);
        // A placeholder that does not fit ends in "…" rather than being clipped.
        if input.content.is_empty() && line.width > bounds.size.width {
            let truncated = cx
                .text_system()
                .line_wrapper(style.font(), font_size)
                .truncate_line(display_text, bounds.size.width, "…", &mut runs);
            line = window
                .text_system()
                .shape_line(truncated, font_size, &runs, None);
        }

        // Centre short text; once it overflows, scroll so the cursor stays visible.
        let cursor_x = line.x_for_index(cursor);
        let origin_x = if line.width <= bounds.size.width {
            if input.centered {
                bounds.left() + (bounds.size.width - line.width) / 2.
            } else {
                bounds.left()
            }
        } else {
            let overflow = (cursor_x - bounds.size.width + px(2.)).max(px(0.));
            bounds.left() - overflow
        };
        let caret_x = if input.content.is_empty() && input.centered {
            bounds.left() + bounds.size.width / 2.
        } else {
            origin_x + cursor_x
        };

        let caret = Bounds::new(
            point(caret_x, bounds.top()),
            size(px(CARET_WIDTH), bounds.size.height),
        );
        let (selection, cursor) = if selected_range.is_empty() {
            // Animate the caret between offsets, as Zed's editor does, but only
            // while the bar is actually drawn: a selection and an unfocused
            // field hide it, and both reset the trail.
            let corners = self.input.update(cx, |input, _| {
                if !input.focus_handle.is_focused(window) {
                    input.cursor_animation.reset();
                    return None;
                }
                input.cursor_animation.update(
                    input.cursor_offset(),
                    caret,
                    CursorViewport::new(origin_x, bounds, window.line_height()),
                    Instant::now(),
                )
            });
            if corners.is_some() {
                window.request_animation_frame();
            }
            (
                None,
                Some(CursorPaint {
                    target: fill(caret, theme.accent),
                    color: theme.accent,
                    corners,
                }),
            )
        } else {
            self.input
                .update(cx, |input, _| input.cursor_animation.reset());
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            origin_x + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            origin_x + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    theme.selection,
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            origin_x,
            cursor,
            selection,
            multi: None,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(multi) = prepaint.multi.take() {
            self.paint_multiline(bounds, multi, prepaint.cursor.take(), window, cx);
            return;
        }
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let Some(line) = prepaint.line.take() else {
            return;
        };
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            if let Some(selection) = prepaint.selection.take() {
                window.paint_quad(selection)
            }
            line.paint(
                point(prepaint.origin_x, bounds.top()),
                window.line_height(),
                window,
                cx,
            )
            .ok();
            if focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                paint_cursor(cursor, window);
            }
        });

        let origin_x = prepaint.origin_x;
        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.last_origin_x = origin_x;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .w_full()
            .key_context(if self.multiline {
                MULTILINE_KEY_CONTEXT
            } else {
                CONTEXT
            })
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_start))
            .on_action(cx.listener(Self::delete_to_end))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_to_start))
            .on_action(cx.listener(Self::select_to_end))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Action as _, KeyContext, Keymap, Keystroke};

    use super::*;

    /// The action `keys` runs in a field with this key context, as GPUI's
    /// dispatcher would pick it.
    fn action_for(keys: &str, context: &str) -> Option<&'static str> {
        let keymap = Keymap::new(bindings());
        let contexts = [KeyContext::parse(context).unwrap()];
        let keystroke = Keystroke::parse(keys).unwrap();
        let (matched, _) = keymap.bindings_for_input(&[keystroke], &contexts);
        matched.first().map(|binding| binding.action().name())
    }

    #[test]
    fn enter_sends_and_shift_enter_breaks_the_line_in_the_composer() {
        assert_eq!(
            action_for("enter", MULTILINE_KEY_CONTEXT),
            Some(Submit.name())
        );
        assert_eq!(
            action_for("shift-enter", MULTILINE_KEY_CONTEXT),
            Some(Newline.name())
        );
    }

    #[test]
    fn enter_confirms_an_ime_composition_instead_of_submitting() {
        assert_eq!(enter(true), Enter::ConfirmComposition);
        assert_eq!(enter(false), Enter::Submit);
    }

    #[test]
    fn esc_is_left_to_the_view_around_the_field() {
        // The composer (cancel reply) and search (dismiss) bind it themselves.
        assert_eq!(action_for("escape", MULTILINE_KEY_CONTEXT), None);
        assert_eq!(action_for("escape", CONTEXT), None);
    }

    #[test]
    fn single_line_fields_never_get_a_line_break() {
        assert_eq!(action_for("enter", CONTEXT), Some(Submit.name()));
        assert_eq!(action_for("shift-enter", CONTEXT), None);
    }

    #[test]
    fn up_and_down_move_between_lines_only_in_a_multiline_field() {
        assert_eq!(action_for("up", MULTILINE_KEY_CONTEXT), Some(Up.name()));
        assert_eq!(action_for("down", MULTILINE_KEY_CONTEXT), Some(Down.name()));
        assert_eq!(
            action_for("shift-up", MULTILINE_KEY_CONTEXT),
            Some(SelectUp.name())
        );
        assert_eq!(
            action_for("shift-down", MULTILINE_KEY_CONTEXT),
            Some(SelectDown.name())
        );
        // Search keeps up/down for its results.
        assert_eq!(action_for("up", CONTEXT), None);
        assert_eq!(action_for("down", CONTEXT), None);
    }

    #[test]
    fn the_standard_editing_keys_still_work_in_a_multiline_field() {
        assert_eq!(
            action_for("secondary-v", MULTILINE_KEY_CONTEXT),
            Some(Paste.name())
        );
        assert_eq!(
            action_for("secondary-z", MULTILINE_KEY_CONTEXT),
            Some(Undo.name())
        );
        assert_eq!(
            action_for("backspace", MULTILINE_KEY_CONTEXT),
            Some(Backspace.name())
        );
    }
}
