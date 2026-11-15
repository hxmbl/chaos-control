//! Markdown rendering for conversation messages.
//!
//! Renders CommonMark-ish markdown to gpui elements for display in the chat.
//! The composer remains plain-text source editing; this renderer is used only
//! for message display so sent content is shown with formatting while the
//! stored payload stays unchanged.
//!
//! Mention highlighting (known `@members`/broadcasts) is applied to rendered
//! text so it stays consistent with plain-text rendering.

use std::{
    collections::HashMap as StdHashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use gpui::{
    Animation, AnimationExt, AnyElement, ClipboardItem, ElementId, FontStyle, FontWeight, Hsla,
    SharedString, StrikethroughStyle, StyledText, TextRun, UnderlineStyle, div, prelude::*, px,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle as SynFontStyle, ThemeSet},
    parsing::SyntaxSet,
};
use ui::{
    IconName, Tooltip, TypeStyleExt, icon,
    tokens::{icon as icons, radius, size, space, text},
};

use crate::mention_highlight::{self, KnownMentions};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link_dest: Option<String>,
}

impl InlineStyle {
    fn is_link(&self) -> bool {
        self.link_dest.is_some()
    }
}

#[derive(Clone)]
struct Span {
    text: String,
    style: InlineStyle,
}

fn syntect_assets() -> &'static (
    SyntaxSet,
    syntect::highlighting::Theme,
    syntect::highlighting::Theme,
) {
    use std::{io::Cursor, sync::OnceLock};
    static ASSETS: OnceLock<(
        SyntaxSet,
        syntect::highlighting::Theme,
        syntect::highlighting::Theme,
    )> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let ss = SyntaxSet::load_defaults_newlines();
        let mut dark = ThemeSet::load_from_reader(&mut Cursor::new(include_bytes!(
            "../themes/tokyonight_night.tmTheme"
        ) as &[u8]))
        .unwrap_or_else(|_| {
            ThemeSet::load_defaults()
                .themes
                .remove("base16-ocean.dark")
                .expect("default theme")
        });
        let mut light = ThemeSet::load_from_reader(&mut Cursor::new(include_bytes!(
            "../themes/tokyonight_day.tmTheme"
        ) as &[u8]))
        .unwrap_or_else(|_| {
            ThemeSet::load_defaults()
                .themes
                .remove("InspiredGitHub")
                .expect("default theme")
        });
        let _ = (&mut dark, &mut light);
        (ss, dark, light)
    })
}

static COPIED_BLOCKS: OnceLock<Mutex<StdHashMap<String, Instant>>> = OnceLock::new();

const COPIED_FOR: Duration = Duration::from_secs(3);

fn is_block_copied(key: &str) -> bool {
    COPIED_BLOCKS
        .get_or_init(|| Mutex::new(StdHashMap::new()))
        .lock()
        .ok()
        .and_then(|m| m.get(key).copied())
        .is_some_and(|until| Instant::now() < until)
}

fn mark_block_copied(key: String) {
    if let Ok(mut m) = COPIED_BLOCKS
        .get_or_init(|| Mutex::new(StdHashMap::new()))
        .lock()
    {
        m.insert(key, Instant::now() + COPIED_FOR);
    }
}

fn clear_block_copied(key: &str) {
    if let Ok(mut m) = COPIED_BLOCKS
        .get_or_init(|| Mutex::new(StdHashMap::new()))
        .lock()
    {
        m.remove(key);
    }
}

#[derive(Clone, Debug)]
struct SyntaxSpan {
    len: usize,
    color: Hsla,
    bold: bool,
    italic: bool,
}

fn highlight_code_runs(
    code: &str,
    lang: Option<&str>,
    base_style: &gpui::TextStyle,
    _base_color: Hsla,
    is_dark: bool,
) -> Vec<Vec<TextRun>> {
    #[derive(Clone, Eq, Hash, PartialEq)]
    struct Key(String, Option<String>, bool);
    static CACHE: OnceLock<Mutex<StdHashMap<Key, Vec<Vec<SyntaxSpan>>>>> = OnceLock::new();
    const MAX_CACHE_ENTRIES: usize = 256;

    let key = Key(code.to_string(), lang.map(|s| s.to_string()), is_dark);
    let syntax_lines: Vec<Vec<SyntaxSpan>> = if let Some(hit) = CACHE
        .get_or_init(|| Mutex::new(StdHashMap::new()))
        .lock()
        .ok()
        .and_then(|m| m.get(&key).cloned())
    {
        hit
    } else {
        let (ss, dark, light) = syntect_assets();
        let first = code.lines().next().unwrap_or("");
        let syntax = lang
            .and_then(|l| {
                ss.find_syntax_by_token(l)
                    .or_else(|| ss.find_syntax_by_extension(l))
                    .or_else(|| ss.find_syntax_by_name(l))
            })
            .or_else(|| ss.find_syntax_by_first_line(first))
            .or_else(|| {
                let c = code;
                if c.contains("fn ") && c.contains('{') {
                    ss.find_syntax_by_extension("rs")
                } else if c.contains("const ")
                    || c.contains("function ")
                    || c.contains("=>")
                    || c.contains("export ")
                {
                    ss.find_syntax_by_extension("js")
                } else if c.contains("def ") && c.contains(':') {
                    ss.find_syntax_by_extension("py")
                } else if c.contains("#include") {
                    ss.find_syntax_by_extension("c")
                } else if c.contains("<div") || c.contains("</") || c.contains("<html") {
                    ss.find_syntax_by_extension("html")
                } else if c.trim_start().starts_with('{') && c.contains(':') {
                    ss.find_syntax_by_extension("json")
                } else {
                    None
                }
            })
            .unwrap_or_else(|| ss.find_syntax_plain_text());
        let theme = if is_dark { dark } else { light };
        let mut h = HighlightLines::new(syntax, theme);
        let mut out = Vec::new();
        // Split lines including terminators, supporting \n, \r\n, and bare \r.
        let mut start = 0usize;
        let b = code.as_bytes();
        while start < b.len() {
            let mut end = start;
            while end < b.len() {
                if b[end] == b'\n' {
                    end += 1;
                    break;
                }
                if b[end] == b'\r' {
                    end += 1;
                    if end < b.len() && b[end] == b'\n' {
                        end += 1;
                    }
                    break;
                }
                end += 1;
            }
            let line = &code[start..end];
            let ranges = h.highlight_line(line, ss).unwrap_or_default();
            let mut spans = Vec::new();
            for (style, text) in ranges {
                let text = text.strip_suffix('\n').unwrap_or(text);
                let text = text.strip_suffix('\r').unwrap_or(text);
                if text.is_empty() {
                    continue;
                }
                let rgba = gpui::Rgba {
                    r: style.foreground.r as f32 / 255.0,
                    g: style.foreground.g as f32 / 255.0,
                    b: style.foreground.b as f32 / 255.0,
                    a: style.foreground.a as f32 / 255.0,
                };
                spans.push(SyntaxSpan {
                    len: text.len(),
                    color: rgba.into(),
                    bold: style.font_style.contains(SynFontStyle::BOLD),
                    italic: style.font_style.contains(SynFontStyle::ITALIC),
                });
            }
            out.push(spans);
            start = end;
        }
        if code.is_empty() {
            out.push(Vec::new());
        }
        if let Ok(mut m) = CACHE.get_or_init(|| Mutex::new(StdHashMap::new())).lock() {
            if m.len() >= MAX_CACHE_ENTRIES {
                m.clear();
            }
            m.insert(key, out.clone());
        }
        out
    };

    syntax_lines
        .into_iter()
        .map(|spans| {
            spans
                .into_iter()
                .map(|span| {
                    let mut font = base_style.font();
                    if span.bold {
                        font.weight = FontWeight::BOLD;
                    }
                    if span.italic {
                        font.style = FontStyle::Italic;
                    }
                    TextRun {
                        len: span.len,
                        font,
                        color: span.color,
                        background_color: base_style.background_color,
                        underline: base_style.underline,
                        strikethrough: base_style.strikethrough,
                    }
                })
                .collect()
        })
        .collect()
}

fn is_safe_link_dest(dest: &str) -> bool {
    let d = dest.trim();
    if d.starts_with("https://") || d.starts_with("http://") || d.starts_with("mailto:") {
        return true;
    }
    // Allow relative/fragment links as non-navigating-safe? only open explicit safe schemes.
    false
}

pub fn render_message_markdown(
    source: &str,
    known: &KnownMentions,
    text_color: Hsla,
    base_style: &gpui::TextStyle,
    theme: &ui::Theme,
    context_key: &str,
) -> AnyElement {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(source, opts);
    let mut renderer = Renderer::new(known, text_color, theme, base_style, context_key);
    renderer.run(parser);
    renderer.finish().into_any_element()
}

#[derive(Clone)]
struct ListState {
    ordered: bool,
    start: u64,
}

type ListStack = Vec<(ListState, Vec<(Option<String>, Vec<AnyElement>)>)>;

struct Renderer<'a> {
    known: &'a KnownMentions,
    text_color: Hsla,
    theme: &'a ui::Theme,
    base_style: &'a gpui::TextStyle,
    blocks: Vec<AnyElement>,
    para_spans: Vec<Span>,
    style_stack: Vec<InlineStyle>,
    blockquote_depth: usize,
    in_code_block: bool,
    code_block_text: String,
    code_block_lang: Option<String>,
    pending_heading: Option<HeadingLevel>,
    // tables
    in_table: bool,
    in_table_head: bool,
    table_alignments: Vec<pulldown_cmark::Alignment>,
    table_header: Vec<AnyElement>,
    table_rows: Vec<Vec<AnyElement>>,
    current_table_row: Vec<AnyElement>,
    in_table_cell: bool,
    list_stack: ListStack,
    item_block_stack: Vec<Vec<AnyElement>>,
    item_marker_stack: Vec<Option<String>>,
    in_list_item: usize,
    image_span_starts: Vec<usize>,
    code_block_index: usize,
    context_key: String,
}

impl<'a> Renderer<'a> {
    fn new(
        known: &'a KnownMentions,
        text_color: Hsla,
        theme: &'a ui::Theme,
        base_style: &'a gpui::TextStyle,
        context_key: &str,
    ) -> Self {
        Self {
            known,
            text_color,
            theme,
            base_style,
            blocks: Vec::new(),
            para_spans: Vec::new(),
            style_stack: vec![InlineStyle::default()],
            blockquote_depth: 0,
            in_code_block: false,
            code_block_text: String::new(),
            code_block_lang: None,
            pending_heading: None,
            in_table: false,
            in_table_head: false,
            table_alignments: Vec::new(),
            table_header: Vec::new(),
            table_rows: Vec::new(),
            current_table_row: Vec::new(),
            in_table_cell: false,
            list_stack: Vec::new(),
            item_block_stack: Vec::new(),
            item_marker_stack: Vec::new(),
            in_list_item: 0,
            code_block_index: 0,
            context_key: context_key.to_string(),
            image_span_starts: Vec::new(),
        }
    }

    fn current_style(&self) -> InlineStyle {
        self.style_stack.last().cloned().unwrap_or_default()
    }

    fn push_text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_block_text.push_str(text);
            return;
        }
        if text.is_empty() {
            return;
        }
        let style = self.current_style();
        if let Some(last) = self.para_spans.last_mut()
            && last.style == style
        {
            last.text.push_str(text);
            return;
        }
        self.para_spans.push(Span {
            text: text.to_string(),
            style,
        });
    }

    fn flush_paragraph(&mut self) {
        if self.para_spans.is_empty() {
            self.pending_heading = None;
            return;
        }
        let spans = std::mem::take(&mut self.para_spans);
        let heading = self.pending_heading.take();
        let element = self.render_inline_paragraph(spans, heading);
        if self.in_table_cell {
            self.current_table_row.push(element);
            return;
        }
        self.push_block(element);
    }

    fn push_block(&mut self, element: AnyElement) {
        let element = self.wrap_blockquote(element);
        if self.in_list_item > 0
            && let Some(item) = self.item_block_stack.last_mut()
        {
            item.push(element);
            return;
        }
        self.blocks.push(element);
    }

    fn wrap_blockquote(&self, mut element: AnyElement) -> AnyElement {
        let mut depth = self.blockquote_depth;
        while depth > 0 {
            let t = self.theme;
            element = div()
                .pl(space::X3)
                .border_l_2()
                .border_color(t.line_strong)
                .child(element)
                .into_any_element();
            depth -= 1;
        }
        element
    }

    fn render_inline_paragraph(
        &mut self,
        spans: Vec<Span>,
        heading: Option<HeadingLevel>,
    ) -> AnyElement {
        let mut full = String::new();
        for s in &spans {
            full.push_str(&s.text);
        }
        if full.is_empty() {
            return div().into_any_element();
        }
        let mention_spans =
            mention_highlight::mention_highlights(&full, self.known, self.theme.appearance);
        let base_style = self.base_style;
        // If no links, render as single StyledText for proper text flow.
        let has_links = spans.iter().any(|s| s.style.is_link());
        let mut container = div().w_full().min_w_0().text_color(self.text_color);
        if let Some(level) = heading {
            let (ts, weight, color) = match level {
                HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3 => {
                    (text::BODY, FontWeight::BOLD, self.theme.ink)
                }
                _ => (text::BODY, text::MEDIUM, self.theme.ink),
            };
            container = container
                .type_style(ts)
                .font_weight(weight)
                .text_color(color);
        }
        if !has_links {
            let mut runs: Vec<TextRun> = Vec::new();
            let mut offset = 0usize;
            for span in &spans {
                let start = offset;
                let end = offset + span.text.len();
                self.push_styled_runs(
                    &full,
                    base_style,
                    start,
                    end,
                    span.style.clone(),
                    &mention_spans,
                    &mut runs,
                );
                offset = end;
            }
            let text_el = StyledText::new(SharedString::from(full)).with_runs(runs);
            return container
                .whitespace_normal()
                .child(text_el)
                .into_any_element();
        }

        // With links: render segments so links can have tooltips.
        container = container.flex().flex_wrap().items_baseline();
        let mut offset = 0usize;
        let mut link_idx = 0usize;
        for span in &spans {
            let start = offset;
            let end = offset + span.text.len();
            let seg_text = &full[start..end];
            if seg_text.is_empty() {
                offset = end;
                continue;
            }
            let mut runs: Vec<TextRun> = Vec::new();
            self.push_styled_runs(
                &full,
                base_style,
                start,
                end,
                span.style.clone(),
                &mention_spans,
                &mut runs,
            );
            let styled = StyledText::new(SharedString::from(seg_text.to_string())).with_runs(runs);
            if let Some(dest) = &span.style.link_dest {
                let is_safe = is_safe_link_dest(dest);
                let id = ElementId::Name(format!("md-link-{start}-{link_idx}").into());
                link_idx += 1;
                let tip = dest.clone();
                let mut link_el = div().id(id).tooltip(Tooltip::text(tip));
                if is_safe {
                    let url = dest.clone();
                    link_el = link_el.cursor_pointer().on_click(move |_, _, cx| {
                        cx.open_url(&url);
                    });
                }
                container = container.child(link_el.child(styled));
            } else {
                container = container.child(div().whitespace_normal().child(styled));
            }
            offset = end;
        }
        container.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn push_styled_runs(
        &self,
        full: &str,
        base: &gpui::TextStyle,
        start: usize,
        end: usize,
        style: InlineStyle,
        mention_spans: &[(std::ops::Range<usize>, gpui::HighlightStyle)],
        runs: &mut Vec<TextRun>,
    ) {
        if start >= end {
            return;
        }
        let mut cuts: Vec<usize> = vec![start, end];
        for (r, _) in mention_spans {
            if r.end <= start || r.start >= end {
                continue;
            }
            cuts.push(r.start.max(start));
            cuts.push(r.end.min(end));
        }
        cuts.sort_unstable();
        cuts.dedup();
        for w in cuts.windows(2) {
            let a = w[0];
            let b = w[1];
            if a >= b || !full.is_char_boundary(a) || !full.is_char_boundary(b) {
                continue;
            }
            let mention = mention_spans
                .iter()
                .find(|(r, _)| r.start <= a && r.end >= b)
                .map(|(_, h)| *h);
            let mut font = base.font();
            if style.bold {
                font.weight = FontWeight::BOLD;
            }
            if style.italic {
                font.style = FontStyle::Italic;
            }
            let mut color = if style.is_link() {
                self.theme.accent
            } else {
                self.text_color
            };
            let mut bg = base.background_color;
            let mut underline = base.underline;
            let mut strike = base.strikethrough;
            if style.strike {
                strike = Some(StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color),
                });
            }
            if style.is_link() {
                underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(color),
                    wavy: false,
                });
            }
            if style.code {
                bg = Some(self.theme.sunken);
            }
            if let Some(h) = mention {
                if let Some(c) = h.color {
                    color = c;
                }
                if let Some(wt) = h.font_weight {
                    font.weight = wt;
                }
                if let Some(st) = h.font_style {
                    font.style = st;
                }
                if let Some(bgc) = h.background_color {
                    bg = Some(bgc);
                }
                if let Some(u) = h.underline {
                    underline = Some(u);
                }
                if let Some(s) = h.strikethrough {
                    strike = Some(s);
                }
                if let Some(fade) = h.fade_out {
                    color.fade_out(fade);
                }
            }
            runs.push(TextRun {
                len: b - a,
                font,
                color,
                background_color: bg,
                underline,
                strikethrough: strike,
            });
        }
    }

    fn run(&mut self, parser: Parser<'_>) {
        for event in parser {
            match event {
                Event::Start(tag) => self.start_tag(tag),
                Event::End(tag) => self.end_tag(tag),
                Event::Text(t) => self.push_text(&t),
                Event::Code(t) => {
                    let mut s = self.current_style();
                    s.code = true;
                    self.para_spans.push(Span {
                        text: t.to_string(),
                        style: s,
                    });
                }
                Event::SoftBreak => self.push_text(" "),
                Event::HardBreak => self.push_text("\n"),
                Event::Rule => {
                    self.flush_paragraph();
                    let t = self.theme;
                    self.push_block(
                        div()
                            .w_full()
                            .h(px(1.))
                            .my(space::X1)
                            .bg(t.line)
                            .into_any_element(),
                    );
                }
                Event::Html(t) | Event::InlineHtml(t) => self.push_text(&t),
                Event::FootnoteReference(t) => self.push_text(&format!("[{t}]")),
                Event::TaskListMarker(checked) => {
                    if let Some(marker) = self.item_marker_stack.last_mut() {
                        *marker = Some(if checked {
                            "☑".to_string()
                        } else {
                            "☐".to_string()
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.pending_heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.flush_paragraph();
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_paragraph();
                self.in_code_block = true;
                self.code_block_text.clear();
                self.code_block_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(info) => {
                        let lang = info.split_whitespace().next().unwrap_or("").trim();
                        if lang.is_empty() {
                            None
                        } else {
                            Some(lang.to_string())
                        }
                    }
                    pulldown_cmark::CodeBlockKind::Indented => None,
                };
            }
            Tag::List(start) => {
                self.flush_paragraph();
                self.list_stack.push((
                    ListState {
                        ordered: start.is_some(),
                        start: start.unwrap_or(1),
                    },
                    Vec::new(),
                ));
            }
            Tag::Item => {
                self.in_list_item += 1;
                self.item_block_stack.push(Vec::new());
                self.item_marker_stack.push(None);
            }
            Tag::Table(alignments) => {
                self.flush_paragraph();
                self.in_table = true;
                self.in_table_head = false;
                self.table_alignments = alignments;
                self.table_header.clear();
                self.table_rows.clear();
            }
            Tag::TableHead => {
                self.in_table_head = true;
                self.current_table_row.clear();
            }
            Tag::TableRow => {
                self.current_table_row.clear();
            }
            Tag::TableCell => {
                self.in_table_cell = true;
                self.para_spans.clear();
            }
            Tag::Emphasis => {
                let mut s = self.current_style();
                s.italic = true;
                self.style_stack.push(s);
            }
            Tag::Strong => {
                let mut s = self.current_style();
                s.bold = true;
                self.style_stack.push(s);
            }
            Tag::Strikethrough => {
                let mut s = self.current_style();
                s.strike = true;
                self.style_stack.push(s);
            }
            Tag::Link { dest_url, .. } => {
                let mut s = self.current_style();
                s.link_dest = Some(dest_url.to_string());
                self.style_stack.push(s);
            }
            Tag::Image { .. } => {
                // Collect alt text; emit a single representation on TagEnd::Image.
                self.image_span_starts.push(self.para_spans.len());
                let mut s = self.current_style();
                s.link_dest = None;
                self.style_stack.push(s);
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => self.flush_paragraph(),
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                let code = std::mem::take(&mut self.code_block_text);
                let lang = self.code_block_lang.take();
                let t = self.theme;
                let is_dark = matches!(t.appearance, ui::Appearance::Dark);
                let code_trimmed = code.trim_end_matches('\n').to_string();
                let highlighted = highlight_code_runs(
                    &code_trimmed,
                    lang.as_deref(),
                    self.base_style,
                    t.ink,
                    is_dark,
                );
                let mut lines_col = div()
                    .w_full()
                    .px(space::X2)
                    .py(space::X1_5)
                    .text_color(t.ink)
                    .type_style(text::SMALL)
                    .flex()
                    .flex_col();
                if code_trimmed.is_empty() {
                    lines_col = lines_col.child(SharedString::from(" "));
                } else {
                    let lines: Vec<&str> = code_trimmed.split('\n').collect();
                    for (i, line) in lines.iter().enumerate() {
                        let display: SharedString = if line.is_empty() {
                            " ".into()
                        } else {
                            (*line).to_string().into()
                        };
                        if let Some(runs) = highlighted.get(i) {
                            if runs.is_empty() || line.is_empty() {
                                lines_col =
                                    lines_col.child(div().w_full().min_w_0().child(display));
                            } else {
                                let total: usize = runs.iter().map(|r| r.len).sum();
                                let mut runs = runs.clone();
                                if total != display.len() && !runs.is_empty() {
                                    runs = vec![TextRun {
                                        len: display.len(),
                                        font: self.base_style.font(),
                                        color: t.ink,
                                        background_color: self.base_style.background_color,
                                        underline: self.base_style.underline,
                                        strikethrough: self.base_style.strikethrough,
                                    }];
                                }
                                let styled = StyledText::new(display).with_runs(runs);
                                lines_col = lines_col.child(div().w_full().min_w_0().child(styled));
                            }
                        } else {
                            lines_col = lines_col
                                .child(div().w_full().min_w_0().whitespace_normal().child(display));
                        }
                    }
                }

                let idx = self.code_block_index;
                self.code_block_index += 1;
                let copy_text = code_trimmed.clone();
                let copy_key = format!("{}::code::{}", self.context_key, idx);
                let copied = is_block_copied(&copy_key);
                let (btn_icon, tip) = if copied {
                    (IconName::Check, "Copied")
                } else {
                    (IconName::CopySimple, "Copy code")
                };
                let key_for_click = copy_key.clone();
                let icon_fade_id =
                    SharedString::from(format!("md-code-copy-fade-{copy_key}-{copied}"));
                let icon_wrap_id =
                    SharedString::from(format!("md-code-copy-icon-{copy_key}-{copied}"));
                let btn_id = SharedString::from(format!("md-code-copy-{copy_key}"));
                let icon_color = t.ink;
                let icon_el = div().id(icon_wrap_id).with_animation(
                    icon_fade_id,
                    Animation::new(Duration::from_millis(180)),
                    move |el, delta| {
                        el.opacity(delta)
                            .child(icon(btn_icon, icons::SM, icon_color))
                    },
                );
                let copy_btn = div()
                    .id(btn_id)
                    .size(size::BUTTON_SM)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(radius::SM)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.hover))
                    .tooltip(Tooltip::text(tip))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                        mark_block_copied(key_for_click.clone());
                        cx.refresh_windows();
                        let k = key_for_click.clone();
                        cx.spawn(async move |cx| {
                            cx.background_executor().timer(COPIED_FOR).await;
                            clear_block_copied(&k);
                            let _ = cx.update(|cx| cx.refresh_windows());
                        })
                        .detach();
                    })
                    .child(icon_el);

                let header = div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(space::X2)
                    .pt(space::X1)
                    .child(
                        div()
                            .type_style(text::CAPTION)
                            .text_color(t.muted)
                            .when_some(lang.filter(|l| !l.is_empty()), |d, lang| {
                                d.child(SharedString::from(lang))
                            }),
                    )
                    .child(copy_btn);

                let block = div()
                    .w_full()
                    .rounded(radius::SM)
                    .bg(t.sunken)
                    .flex()
                    .flex_col()
                    .child(header)
                    .child(lines_col);
                self.push_block(block.into_any_element());
            }
            TagEnd::List(_) => {
                self.flush_paragraph();
                if let Some((state, items)) = self.list_stack.pop() {
                    let t = self.theme;
                    let mut col = div().flex().flex_col().gap(space::X0_5);
                    for (i, (task_marker, item_blocks)) in items.into_iter().enumerate() {
                        let marker = task_marker.unwrap_or_else(|| {
                            if state.ordered {
                                format!("{}.", state.start + i as u64)
                            } else {
                                "•".to_string()
                            }
                        });
                        let mut item_col =
                            div().flex().flex_col().gap(space::X0_5).min_w_0().flex_1();
                        for b in item_blocks {
                            item_col = item_col.child(b);
                        }
                        let row = div()
                            .flex()
                            .items_start()
                            .gap(space::X2)
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(t.muted)
                                    .child(SharedString::from(marker)),
                            )
                            .child(item_col);
                        col = col.child(row);
                    }
                    self.push_block(col.into_any_element());
                }
            }
            TagEnd::Item => {
                self.flush_paragraph();
                let marker = self.item_marker_stack.pop().flatten();
                if let Some(item_blocks) = self.item_block_stack.pop()
                    && let Some((_, items)) = self.list_stack.last_mut()
                {
                    items.push((marker, item_blocks));
                }
                self.in_list_item = self.in_list_item.saturating_sub(1);
            }
            TagEnd::TableCell => {
                self.flush_paragraph();
                self.in_table_cell = false;
            }
            TagEnd::TableRow => {
                if self.in_table_head {
                    self.table_header = std::mem::take(&mut self.current_table_row);
                } else {
                    self.table_rows
                        .push(std::mem::take(&mut self.current_table_row));
                }
            }
            TagEnd::TableHead => {
                self.in_table_head = false;
            }
            TagEnd::Table => {
                self.in_table = false;
                let t = self.theme;
                let alignments = std::mem::take(&mut self.table_alignments);
                let mut table = div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(t.line)
                    .rounded(radius::SM);
                let cell_div = |idx: usize, cell: AnyElement, bg: bool| {
                    let mut c = div()
                        .flex_1()
                        .min_w_0()
                        .px(space::X2)
                        .py(space::X1)
                        .border_r_1()
                        .border_color(t.line)
                        .flex();
                    c = match alignments
                        .get(idx)
                        .copied()
                        .unwrap_or(pulldown_cmark::Alignment::None)
                    {
                        pulldown_cmark::Alignment::Center => c.justify_center(),
                        pulldown_cmark::Alignment::Right => c.justify_end(),
                        pulldown_cmark::Alignment::Left | pulldown_cmark::Alignment::None => {
                            c.justify_start()
                        }
                    };
                    if bg {
                        c = c.bg(t.sunken);
                    }
                    c.child(cell)
                };
                if !self.table_header.is_empty() {
                    let mut row = div().w_full().flex();
                    for (idx, cell) in std::mem::take(&mut self.table_header)
                        .into_iter()
                        .enumerate()
                    {
                        row = row.child(cell_div(idx, cell, true));
                    }
                    table = table.child(row);
                }
                for body_row in std::mem::take(&mut self.table_rows) {
                    let mut row = div().w_full().flex().border_t_1().border_color(t.line);
                    for (idx, cell) in body_row.into_iter().enumerate() {
                        row = row.child(cell_div(idx, cell, false));
                    }
                    table = table.child(row);
                }
                self.push_block(table.into_any_element());
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                if self.style_stack.len() > 1 {
                    self.style_stack.pop();
                }
            }
            TagEnd::Image => {
                if self.style_stack.len() > 1 {
                    self.style_stack.pop();
                }
                if let Some(start) = self.image_span_starts.pop() {
                    let alt: String = self.para_spans.drain(start..).map(|s| s.text).collect();
                    let alt = alt.trim();
                    let repr = if alt.is_empty() {
                        "[image]".to_string()
                    } else {
                        alt.to_string()
                    };
                    self.para_spans.push(Span {
                        text: repr,
                        style: self.current_style(),
                    });
                }
            }
            _ => {}
        }
    }

    fn finish(mut self) -> AnyElement {
        self.flush_paragraph();
        if self.blocks.is_empty() {
            return div().into_any_element();
        }
        let mut col = div().w_full().min_w_0().flex().flex_col().gap(space::X1);
        for b in self.blocks {
            col = col.child(b);
        }
        col.into_any_element()
    }
}

/// Highlight styles for markdown syntax markers (delimiters/punctuation) in `text`,
/// dimmed so the content stands out while markers remain visible.
///
/// Uses a parser-aware pass to avoid styling inside code spans/blocks and to only
/// dim markers that belong to recognized markdown constructs where possible.
pub fn markdown_marker_highlights(
    text: &str,
    theme: &ui::Theme,
) -> Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> {
    use gpui::HighlightStyle;
    use std::collections::HashSet;

    let dim = theme.faint;
    let style = HighlightStyle {
        color: Some(dim),
        ..Default::default()
    };
    let mut out: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();

    let mut push = |r: std::ops::Range<usize>| {
        if r.start < r.end
            && r.end <= text.len()
            && text.is_char_boundary(r.start)
            && text.is_char_boundary(r.end)
            && seen.insert((r.start, r.end))
        {
            out.push((r.clone(), style));
        }
    };

    let bytes = text.as_bytes();
    let len = text.len();

    // Protected regions: inline code and fenced/indented code blocks from parser.
    let mut protected: Vec<std::ops::Range<usize>> = Vec::new();
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    for (event, range) in Parser::new_ext(text, opts).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) | Event::End(TagEnd::CodeBlock) => {
                // We still want to dim fence markers on fence lines; handled by line scan,
                // but protect contents by excluding interior heuristics below.
            }
            Event::Code(_) if range.start < range.end && range.end <= len => {
                protected.push(range.clone());
                // dim backtick delimiters of inline code if present at edges
                let s = &text[range.clone()];
                if s.starts_with('`') {
                    let mut t = 0usize;
                    for b in s.bytes() {
                        if b == b'`' {
                            t += 1;
                        } else {
                            break;
                        }
                    }
                    if t > 0 {
                        push(range.start..range.start + t);
                        if s.ends_with('`') && s.len() > t {
                            let mut e = 0usize;
                            for b in s.bytes().rev() {
                                if b == b'`' {
                                    e += 1;
                                } else {
                                    break;
                                }
                            }
                            if e > 0 && range.end >= e {
                                push(range.end - e..range.end);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Add code block body protection by tracking parser code block ranges more explicitly.
    let mut in_code_block = false;
    let mut code_block_start = 0usize;
    for (event, range) in Parser::new_ext(text, opts).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
                code_block_start = range.start;
            }
            Event::End(TagEnd::CodeBlock) => {
                if in_code_block {
                    let end = range.end.min(len);
                    if code_block_start < end {
                        protected.push(code_block_start..end);
                    }
                }
                in_code_block = false;
            }
            _ => {}
        }
    }
    // merge protected ranges
    protected.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for r in protected {
        if let Some(last) = merged.last_mut()
            && r.start <= last.end
        {
            if r.end > last.end {
                last.end = r.end;
            }
            continue;
        }
        merged.push(r);
    }
    let protected = merged;

    let is_protected = |idx: usize| -> bool {
        // idx is byte index
        protected.iter().any(|r| idx >= r.start && idx < r.end)
    };

    // Line-start markers (headings, blockquotes, list markers) and fence markers
    let mut i = 0usize;
    while i < len {
        if i == 0 || bytes.get(i.wrapping_sub(1)) == Some(&b'\n') {
            let mut j = i;
            while j < len && bytes[j] == b' ' {
                j += 1;
            }
            // heading hashes
            let mut k = j;
            while k < len && bytes[k] == b'#' && k - j < 6 {
                k += 1;
            }
            if k > j && k < len && bytes[k] == b' ' && !is_protected(j) {
                push(j..k + 1);
            }
            // blockquote
            if j < len && bytes[j] == b'>' && !is_protected(j) {
                let mut k2 = j + 1;
                if k2 < len && bytes[k2] == b' ' {
                    k2 += 1;
                }
                push(j..k2);
            }
            // unordered list marker
            if j + 1 < len
                && (bytes[j] == b'-' || bytes[j] == b'*' || bytes[j] == b'+')
                && bytes[j + 1] == b' '
                && !is_protected(j)
            {
                push(j..j + 2);
            }
            // ordered list marker
            let mut d = j;
            while d < len && bytes[d].is_ascii_digit() {
                d += 1;
            }
            if d > j
                && d + 1 < len
                && (bytes[d] == b'.' || bytes[d] == b')')
                && bytes[d + 1] == b' '
                && !is_protected(j)
            {
                push(j..d + 2);
            }
            // fenced code fence line marker (``` or ~~~)
            if j + 3 <= len
                && !is_protected(j)
                && (&bytes[j..j + 3] == b"```" || &bytes[j..j + 3] == b"~~~")
            {
                push(j..j + 3);
            }
        }
        // advance to next line start
        if let Some(pos) = text[i..].find('\n') {
            i += pos + 1;
        } else {
            break;
        }
    }

    // Emphasis/strike/link/image markers in non-protected regions using a careful scan.
    // We only dim delimiter chars when they are likely markdown syntax (not escaped, not in code),
    // and we avoid naive dimming of standalone underscores/brackets/parens in ordinary text.
    let mut idx = 0usize;
    while idx < len {
        let b = bytes[idx];
        if is_protected(idx) {
            idx += 1;
            continue;
        }
        // skip escaped char
        if b == b'\\' {
            idx += 2;
            continue;
        }
        // inline code already handled/protected; but if somehow not, skip backtick runs
        if b == b'`' {
            idx += 1;
            continue;
        }
        // strikethrough ~~
        if b == b'~' && idx + 1 < len && bytes[idx + 1] == b'~' {
            push(idx..idx + 2);
            idx += 2;
            continue;
        }
        // bold ** or __
        if (b == b'*' || b == b'_') && idx + 1 < len && bytes[idx + 1] == b {
            // dim as likely delimiter
            push(idx..idx + 2);
            idx += 2;
            continue;
        }
        // italic * or _ as single delimiter: only when it looks like a delimiter, not intraword underscore
        if b == b'*' {
            push(idx..idx + 1);
            idx += 1;
            continue;
        }
        if b == b'_' {
            // avoid intraword underscores like snake_case
            let prev_alnum = idx > 0
                && text[..idx]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric());
            let next_alnum = text[idx + 1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric());
            if !(prev_alnum && next_alnum) {
                push(idx..idx + 1);
            }
            idx += 1;
            continue;
        }
        // image/link markers only when they form a construct: ![, ], (, )
        if b == b'!' && idx + 1 < len && bytes[idx + 1] == b'[' {
            push(idx..idx + 2);
            idx += 2;
            continue;
        }
        if b == b'[' {
            // dim if there is a matching ] later (rough) - better to check for link/image pattern
            if text[idx..].contains(']') {
                push(idx..idx + 1);
            }
            idx += 1;
            continue;
        }
        if b == b']' {
            // dim if followed by ( or [ for links/images/ref links
            if idx + 1 < len && (bytes[idx + 1] == b'(' || bytes[idx + 1] == b'[') {
                push(idx..idx + 1);
            } else if text[..idx].contains('[') {
                // closing bracket of a construct started earlier
                push(idx..idx + 1);
            }
            idx += 1;
            continue;
        }
        if b == b'(' {
            // dim if preceded by ] (link/image url)
            if idx > 0 && bytes[idx - 1] == b']' {
                push(idx..idx + 1);
            }
            idx += 1;
            continue;
        }
        if b == b')' {
            // dim if there's a ]( before it recently
            if text[..idx].rfind("](").is_some() {
                push(idx..idx + 1);
            }
            idx += 1;
            continue;
        }
        // advance by char
        if let Some(ch) = text[idx..].chars().next() {
            idx += ch.len_utf8();
        } else {
            idx += 1;
        }
    }

    out.sort_by_key(|(r, _)| (r.start, r.end));
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TextStyle, px};

    fn base_style() -> TextStyle {
        TextStyle {
            font_family: "test".into(),
            font_size: px(15.).into(),
            ..Default::default()
        }
    }

    fn render_ok(source: &str) {
        let theme = ui::Theme::light();
        let known = KnownMentions::new();
        let _ = render_message_markdown(
            source,
            &known,
            theme.ink_soft,
            &base_style(),
            &theme,
            "test",
        );
    }

    #[test]
    fn common_formatting_renders() {
        render_ok("**bold** *italic* ~~strike~~ `code` [link](https://x.test)");
        render_ok(
            "# Heading\n\nParagraph with **bold** and _italic_.\n\n- item 1\n- item 2\n\n1. one\n2. two\n\n> quote\n\n---\n\n```\nfn main() {}\n```",
        );
    }

    #[test]
    fn incomplete_syntax_renders_without_panic() {
        render_ok("**bold but not closed");
        render_ok("`code without end");
        render_ok("[link text](unterminated");
        render_ok("> quote\n> still quote\n\n- list\n  - nested but maybe odd");
        render_ok("```rust\nfn x() {");
    }

    #[test]
    fn multiline_content_renders() {
        render_ok("line1\nline2\n\npara2\nline3  \nline4");
        render_ok("a\n\n\n\nb");
    }

    #[test]
    fn syntax_highlight_line_endings_preserve_lengths() {
        let theme = ui::Theme::dark();
        let base = base_style();
        let cases = [
            ("fn main() {\n}\n", "rs"),
            ("fn main() {\r\n}\r\n", "rs"),
            ("fn main() {\r}\r", "rs"),
        ];
        for (code, lang) in cases {
            let lines = super::highlight_code_runs(code, Some(lang), &base, theme.ink, true);
            // Expected: split on \n, \r\n, or bare \r, keeping terminators in chunks as our impl does.
            let mut expected: Vec<String> = Vec::new();
            let b = code.as_bytes();
            let mut start = 0usize;
            while start < b.len() {
                let mut end = start;
                while end < b.len() {
                    if b[end] == b'\n' {
                        end += 1;
                        break;
                    }
                    if b[end] == b'\r' {
                        end += 1;
                        if end < b.len() && b[end] == b'\n' {
                            end += 1;
                        }
                        break;
                    }
                    end += 1;
                }
                let mut l = &code[start..end];
                l = l.strip_suffix('\n').unwrap_or(l);
                l = l.strip_suffix('\r').unwrap_or(l);
                expected.push(l.to_string());
                start = end;
            }
            if code.is_empty() {
                expected.push(String::new());
            }
            assert_eq!(lines.len(), expected.len(), "line count for {code:?}");
            for (runs, line) in lines.iter().zip(expected.iter()) {
                let total: usize = runs.iter().map(|r| r.len).sum();
                assert_eq!(
                    total,
                    line.len(),
                    "run length mismatch for {code:?} line {line:?}"
                );
            }
        }
    }

    #[test]
    fn marker_highlights_skip_intraword_underscores_and_plain_parens() {
        let theme = ui::Theme::light();
        let h = super::markdown_marker_highlights("snake_case A (small example)", &theme);
        assert!(
            !h.iter()
                .any(|(r, _)| &"snake_case A (small example)"[r.clone()] == "_"),
            "intraword underscore should not be dimmed: {h:?}"
        );
        assert!(
            !h.iter()
                .any(|(r, _)| &"snake_case A (small example)"[r.clone()] == "("
                    || &"snake_case A (small example)"[r.clone()] == ")"),
            "plain parentheses should not be dimmed: {h:?}"
        );
    }

    #[test]
    fn marker_highlights_do_not_mark_inside_inline_code() {
        let theme = ui::Theme::light();
        let src = "`*not emphasis* _x_ (y)`";
        let h = super::markdown_marker_highlights(src, &theme);
        // Only backtick delimiters at ends should be highlighted; inner markers should not.
        assert!(
            !h.iter().any(|(r, _)| {
                let s = &src[r.clone()];
                s == "*" || s == "_" || s == "(" || s == ")"
            }),
            "markers inside inline code should not be dimmed: {h:?}"
        );
    }

    #[test]
    fn marker_highlights_do_not_mark_inside_fenced_code() {
        let theme = ui::Theme::light();
        let src = "```\n**text** _x_ (y)\n```";
        let h = super::markdown_marker_highlights(src, &theme);
        assert!(
            !h.iter().any(|(r, _)| {
                let s = &src[r.clone()];
                s == "**" || s == "_" || s == "(" || s == ")"
            }),
            "markers inside fenced code content should not be dimmed: {h:?}"
        );
    }

    #[test]
    fn image_alt_text_not_duplicated() {
        render_ok("![diagram](https://example.com/diagram.png)");
        render_ok("![](https://example.com/diagram.png)");
        render_ok("![a *formatted* description](https://example.com/diagram.png)");
    }
}
