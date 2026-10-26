//! Single-line text field.
//!
//! Adapted from GPUI's `examples/input.rs` (Apache-2.0, Copyright Zed Industries):
//! platform IME via `EntityInputHandler`, grapheme-aware cursor movement, mouse
//! selection and clipboard. Added here: placeholder and theme colours, optional
//! centring, a horizontal scroll that keeps the cursor visible, and events.

use std::ops::Range;

use std::time::Instant;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextAlign, TextRun, TruncateFrom, UTF16Selection,
    UnderlineStyle, Window, actions, div, fill, point, prelude::*, px, relative, size,
};

use crate::{
    ActiveTheme,
    editing::{self, EditKind, History, Snapshot},
};

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
    ]
);

const CONTEXT: &str = "TextInput";

/// The editing keys people expect in any text field on their OS. `secondary`
/// is cmd on macOS and ctrl elsewhere. A single-line field has one line, so
/// "line start/end" is the start/end of the text. Up/down are left unbound so
/// the search field can use them for its results.
pub(crate) fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    // Everywhere.
    cx.bind_keys([
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
    ]);
    if cfg!(target_os = "macos") {
        cx.bind_keys([
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
        cx.bind_keys([
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
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// Where the shaped line started on the last paint (centring and scrolling).
    last_origin_x: Pixels,
    is_selecting: bool,
    history: History,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            centered: false,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            last_origin_x: px(0.),
            is_selecting: false,
            history: History::default(),
        }
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

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextInputEvent::Submit);
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
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
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
            self.edit(range, &text.replace('\n', " "), EditKind::Other, cx);
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

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.history.break_group();
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
        self.edit(range, new_text, EditKind::Insert, cx);
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
        self.last_bounds?.localize(&point)?;
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
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
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
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
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
        let mut line =
            window
                .text_system()
                .shape_line(display_text.clone(), font_size, &runs, None);
        // A placeholder that does not fit ends in "…" rather than being clipped.
        if input.content.is_empty() && line.width > bounds.size.width {
            let mut wrapper = cx.text_system().line_wrapper(style.font(), font_size);
            let (truncated, truncated_runs) = wrapper.truncate_line(
                display_text,
                bounds.size.width,
                "…",
                &runs,
                TruncateFrom::End,
            );
            line = window
                .text_system()
                .shape_line(truncated, font_size, &truncated_runs, None);
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

        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(caret_x, bounds.top()),
                        size(px(1.5), bounds.size.height),
                    ),
                    theme.accent,
                )),
            )
        } else {
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
                TextAlign::Left,
                None,
                window,
                cx,
            )
            .ok();
            if focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
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
            .key_context(CONTEXT)
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
