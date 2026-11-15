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
                // Trim a single trailing line terminator for display/copy parity.
                let mut code_trimmed = code.as_str();
                if let Some(s) = code_trimmed.strip_suffix('\n') {
                    code_trimmed = s;
                }
                if let Some(s) = code_trimmed.strip_suffix('\r') {
                    code_trimmed = s;
                }
                let code_trimmed = code_trimmed.to_string();
                let highlighted = highlight_code_runs(
                    &code_trimmed,
                    lang.as_deref(),
                    self.base_style,
                    t.ink,
                    is_dark,
                );
                // Split lines without terminators, supporting \n, \r\n, and bare \r.
                let mut lines: Vec<String> = Vec::new();
                {
                    let b = code_trimmed.as_bytes();
                    let mut start = 0usize;
                    while start < b.len() {
                        let mut end = start;
                        while end < b.len() {
                            if b[end] == b'\n' {
                                break;
                            }
                            if b[end] == b'\r' {
                                break;
                            }
                            end += 1;
                        }
                        lines.push(code_trimmed[start..end].to_string());
                        if end >= b.len() {
                            break;
                        }
                        // skip terminator
                        if b[end] == b'\r' && end + 1 < b.len() && b[end + 1] == b'\n' {
                            start = end + 2;
                        } else {
                            start = end + 1;
                        }
                    }
                    if code_trimmed.is_empty() {
                        lines.clear();
                    }
                }
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
                    for (i, line) in lines.iter().enumerate() {
                        let display: SharedString = if line.is_empty() {
                            " ".into()
                        } else {
                            line.clone().into()
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
                            lines_col = lines_col.child(div().w_full().min_w_0().child(display));
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
/// Parser-aware: uses pulldown-cmark structure to protect code regions and to
/// identify actual link/image constructs; applies conservative delimiter dimming
/// elsewhere to avoid styling ordinary punctuation/identifiers.
pub fn markdown_marker_highlights(
    text: &str,
    theme: &ui::Theme,
) -> Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> {
    use gpui::HighlightStyle;
    use std::collections::{HashMap, HashSet};

    let dim = theme.faint;
    let style = HighlightStyle {
        color: Some(dim),
        ..Default::default()
    };
    let mut out: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let bytes = text.as_bytes();
    let len = text.len();

    let mut push = |r: std::ops::Range<usize>| {
        if r.start < r.end
            && r.end <= len
            && text.is_char_boundary(r.start)
            && text.is_char_boundary(r.end)
            && seen.insert((r.start, r.end))
        {
            out.push((r.clone(), style));
        }
    };

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);

    // Protected regions: inline code and fenced/indented code blocks.
    let mut protected: Vec<std::ops::Range<usize>> = Vec::new();
    let mut linkish: Vec<std::ops::Range<usize>> = Vec::new(); // parsed Link/Image event ranges
    let mut emphasis_tags: HashMap<(usize, usize, u8), usize> = HashMap::new();
    let mut strikethrough_ranges = Vec::new();
    let mut code_block: Option<(std::ops::Range<usize>, bool, Vec<std::ops::Range<usize>>)> = None;
    for (event, range) in Parser::new_ext(text, opts).into_offset_iter() {
        match event {
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
            Event::Start(Tag::Emphasis) if range.start < range.end && range.end <= len => {
                if let Some(marker @ (b'*' | b'_')) = bytes.get(range.start).copied() {
                    *emphasis_tags
                        .entry((range.start, range.end, marker))
                        .or_default() += 1;
                }
            }
            Event::Start(Tag::Strong) if range.start < range.end && range.end <= len => {
                if let Some(marker @ (b'*' | b'_')) = bytes.get(range.start).copied() {
                    *emphasis_tags
                        .entry((range.start, range.end, marker))
                        .or_default() += 2;
                }
            }
            Event::Start(Tag::Strikethrough) if range.start < range.end && range.end <= len => {
                strikethrough_ranges.push(range);
            }
            Event::Start(Tag::Link { .. }) | Event::Start(Tag::Image { .. })
                if range.start < range.end && range.end <= len =>
            {
                linkish.push(range);
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                code_block = Some((
                    range,
                    matches!(kind, pulldown_cmark::CodeBlockKind::Fenced(_)),
                    Vec::new(),
                ));
            }
            Event::Text(_) if code_block.is_some() => {
                if let Some((_, _, contents)) = &mut code_block {
                    contents.push(range);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((block, fenced, contents)) = code_block.take() {
                    if fenced {
                        if let (Some(first), Some(last)) = (contents.first(), contents.last()) {
                            // Code-block offsets include fences; Text offsets isolate their contents.
                            protected.push(first.start..last.end);

                            let opening = block.start..first.start;
                            let opening_start = opening
                                .clone()
                                .find(|idx| !matches!(bytes[*idx], b' ' | b'\t'))
                                .unwrap_or(opening.end);
                            let fence_marker = if opening_start < opening.end {
                                let marker = bytes[opening_start];
                                let run_end = (opening_start..opening.end)
                                    .take_while(|idx| bytes[*idx] == marker)
                                    .last()
                                    .map_or(opening_start, |idx| idx + 1);
                                if run_end - opening_start >= 3 && matches!(marker, b'`' | b'~') {
                                    push(opening_start..run_end);
                                    Some(marker)
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            let closing = last.end..block.end;
                            let closing_end = closing
                                .clone()
                                .rev()
                                .find(|idx| !matches!(bytes[*idx], b' ' | b'\t' | b'\r' | b'\n'))
                                .map_or(closing.start, |idx| idx + 1);
                            if closing.start < closing_end {
                                let marker = bytes[closing_end - 1];
                                let run_start = (closing.start..closing_end)
                                    .rev()
                                    .take_while(|idx| bytes[*idx] == marker)
                                    .last()
                                    .unwrap_or(closing_end - 1);
                                if closing_end - run_start >= 3 && fence_marker == Some(marker) {
                                    push(run_start..closing_end);
                                }
                            }
                        } else {
                            protected.push(block);
                        }
                    } else {
                        protected.push(block);
                    }
                }
            }
            _ => {}
        }
    }

    // Tag event ranges cover full constructs; tag kinds identify active delimiter widths.
    for ((start, end, marker), width) in emphasis_tags {
        let opening_width = (start..end)
            .take_while(|idx| bytes[*idx] == marker)
            .count()
            .min(width);
        let closing_width = (start..end)
            .rev()
            .take_while(|idx| bytes[*idx] == marker)
            .count()
            .min(width);
        if opening_width > 0 && closing_width > 0 {
            push(start..start + opening_width);
            push(end - closing_width..end);
        }
    }
    for range in strikethrough_ranges {
        if range.end - range.start >= 4
            && bytes.get(range.start..range.start + 2) == Some(b"~~")
            && bytes.get(range.end - 2..range.end) == Some(b"~~")
        {
            push(range.start..range.start + 2);
            push(range.end - 2..range.end);
        }
    }

    fn merge_ranges(mut rs: Vec<std::ops::Range<usize>>) -> Vec<std::ops::Range<usize>> {
        rs.sort_by_key(|r| (r.start, r.end));
        let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
        for r in rs {
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
        merged
    }
    let protected = merge_ranges(protected);
    let linkish = merge_ranges(linkish);

    let in_ranges = |idx: usize, ranges: &[std::ops::Range<usize>]| -> bool {
        ranges.iter().any(|r| idx >= r.start && idx < r.end)
    };
    let is_protected = |idx: usize| in_ranges(idx, &protected);
    let in_linkish = |idx: usize| in_ranges(idx, &linkish);

    // Line-start markers and fence markers
    let mut i = 0usize;
    while i < len {
        if i == 0 || bytes.get(i.wrapping_sub(1)) == Some(&b'\n') {
            let mut j = i;
            while j < len && bytes[j] == b' ' {
                j += 1;
            }
            let mut k = j;
            while k < len && bytes[k] == b'#' && k - j < 6 {
                k += 1;
            }
            if k > j && k < len && bytes[k] == b' ' && !is_protected(j) {
                push(j..k + 1);
            }
            if j < len && bytes[j] == b'>' && !is_protected(j) {
                let mut k2 = j + 1;
                if k2 < len && bytes[k2] == b' ' {
                    k2 += 1;
                }
                push(j..k2);
            }
            if j + 1 < len
                && (bytes[j] == b'-' || bytes[j] == b'*' || bytes[j] == b'+')
                && bytes[j + 1] == b' '
                && !is_protected(j)
            {
                // only dim '-'/'+'/'*' as list marker in this list context
                push(j..j + 2);
            }
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
        }
        if let Some(pos) = text[i..].find('\n') {
            i += pos + 1;
        } else {
            break;
        }
    }

    // Inline markers: be conservative to avoid dimming ordinary punctuation/text.
    let mut idx = 0usize;
    while idx < len {
        let b = bytes[idx];
        if is_protected(idx) {
            idx += 1;
            continue;
        }
        if b == b'\\' {
            if idx + 1 < len && bytes[idx + 1].is_ascii_punctuation() {
                idx += 2;
            } else {
                idx += 1;
            }
            continue;
        }
        // Link/image markers: only dim when near/within parser-recognized link/image constructs
        if b == b'!'
            && idx + 1 < len
            && bytes[idx + 1] == b'['
            && (in_linkish(idx) || in_linkish(idx + 1))
        {
            push(idx..idx + 2);
            idx += 2;
            continue;
        }
        if b == b'[' {
            if in_linkish(idx) {
                push(idx..idx + 1);
            }
            idx += 1;
            continue;
        }
        if b == b']' {
            if in_linkish(idx) {
                push(idx..idx + 1);
            }
            idx += 1;
            continue;
        }
        if b == b'(' {
            if idx > 0 && bytes[idx - 1] == b']' && (in_linkish(idx) || in_linkish(idx - 1)) {
                push(idx..idx + 1);
            }
            idx += 1;
            continue;
        }
        if b == b')' {
            if in_linkish(idx) {
                push(idx..idx + 1);
            }
            idx += 1;
            continue;
        }
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

    fn highlighted_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
        let theme = ui::Theme::light();
        let highlights = super::markdown_marker_highlights(source, &theme);
        for (range, _) in &highlights {
            assert!(source.is_char_boundary(range.start));
            assert!(source.is_char_boundary(range.end));
        }
        highlights.iter().map(|(range, _)| range.clone()).collect()
    }

    fn highlighted_slices(source: &str) -> Vec<&str> {
        highlighted_ranges(source)
            .iter()
            .map(|range| &source[range.clone()])
            .collect()
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

    #[test]
    fn marker_highlights_escaped_and_unmatched_markers() {
        assert!(highlighted_slices(r"\*literal\* and \_literal\_").is_empty());
        assert!(highlighted_slices("unmatched * marker and **strong").is_empty());
        assert!(highlighted_slices("2 * 3 = 6; 2 ** 3").is_empty());
        assert_eq!(
            highlighted_slices(r"\\*two slashes* and *valid*"),
            ["*", "*", "*", "*"]
        );
        assert_eq!(highlighted_slices(r"\*escaped\* but *active*"), ["*", "*"]);
    }

    #[test]
    fn marker_highlights_only_parser_recognized_emphasis() {
        assert_eq!(
            highlighted_slices("**bold _nested_** *one* **two**"),
            ["**", "_", "_", "**", "*", "*", "**", "**"]
        );
        assert_eq!(highlighted_slices("***both***"), ["*", "**", "**", "*"]);
        assert_eq!(highlighted_slices("*one*_two_"), ["*", "*", "_", "_"]);
        assert_eq!(highlighted_slices("snake_case and _italic_"), ["_", "_"]);
        render_ok("**bold _nested_** *one* **two**");
    }

    #[test]
    fn marker_highlights_are_utf8_safe_after_backslashes() {
        for prefix in [r"\é", r"\界", r"\🙂"] {
            let source = format!("{prefix} *active*");
            let actual = highlighted_ranges(&source);
            let expected: Vec<_> = source
                .match_indices('*')
                .map(|(start, marker)| start..start + marker.len())
                .collect();
            assert_eq!(actual, expected);
        }
        assert_eq!(highlighted_slices(r"\\*emphasis* \! \["), ["*", "*"]);
    }

    #[test]
    fn marker_highlights_reference_links_and_images() {
        let src = "See [ref][id] and ![alt][img]\n\n[id]: https://example.com\n[img]: https://example.com/i.png";
        assert_eq!(
            highlighted_slices(src),
            ["[", "]", "[", "]", "![", "]", "[", "]"]
        );
        assert_eq!(
            highlighted_slices("![*alt*](https://example.com/image.png)"),
            ["![", "*", "*", "]", "(", ")"]
        );
        render_ok(src);
        render_ok("[ref]: https://example.com\n\n[use][ref]");
    }

    #[test]
    fn marker_highlights_plain_brackets_not_link_syntax() {
        let src = "[see notes] and (small example)";
        assert!(highlighted_slices(src).is_empty());
        assert!(highlighted_slices("plain text, then unrelated ]( later").is_empty());
        assert!(highlighted_slices("![not an image] then unrelated ]( later").is_empty());
        assert_eq!(
            highlighted_slices("[**bold**](https://example.com)"),
            ["[", "**", "**", "]", "(", ")"]
        );
    }

    #[test]
    fn marker_highlights_only_parser_recognized_strikethrough() {
        assert_eq!(highlighted_slices("~~valid~~"), ["~~", "~~"]);
        assert!(highlighted_slices("~~unmatched and ~ also").is_empty());
        assert!(highlighted_slices(r"\~\~escaped\~\~").is_empty());
        assert_eq!(
            highlighted_slices("~~one~~ ~~two~~"),
            ["~~", "~~", "~~", "~~"]
        );
        render_ok("~~valid~~");
    }

    #[test]
    fn marker_highlights_fences_but_protects_code_content() {
        assert_eq!(
            highlighted_slices("```rust\n**inside**\n```"),
            ["```", "```"]
        );
        assert_eq!(highlighted_slices("~~~text\n*inside*\n~~~"), ["~~~", "~~~"]);
        assert!(highlighted_slices("    **indented code**\n").is_empty());
        assert_eq!(highlighted_slices("`*inline code*`"), ["`", "`"]);
    }

    #[test]
    fn safe_link_dest_validation() {
        assert!(super::is_safe_link_dest("https://example.com"));
        assert!(super::is_safe_link_dest("http://example.com"));
        assert!(super::is_safe_link_dest("mailto:test@example.com"));
        assert!(!super::is_safe_link_dest("javascript:alert(1)"));
        assert!(!super::is_safe_link_dest("file:///etc/passwd"));
        assert!(!super::is_safe_link_dest("data:text/html,x"));
        assert!(!super::is_safe_link_dest(""));
    }

    #[test]
    fn links_and_images_render_without_affecting_plain_text() {
        render_ok("Visit [Example](https://example.com) now");
        render_ok("Visit [Example](http://example.com) now");
        render_ok("Email [me](mailto:test@example.com)");
        render_ok("Bad [x](javascript:alert(1)) stays text");
        render_ok("Plain text without links");
        render_ok("[ref](https://example.com/path)");
    }

    #[test]
    fn code_blocks_preserve_structure_in_both_paths() {
        // highlighted (known language), plain fallback/unsupported, indented, and whitespace cases
        render_ok("```rust\n    indented\n        more indented\n```");
        render_ok("```text\none    two      three\n```");
        render_ok("```text\n  leading spaces\ntrailing spaces  \n```");
        render_ok("```\n  indented\n\tmixed\n```");
        render_ok("    indented code block\n        more");
        render_ok("```unknownlang\nfn main() {}\n\n    spaces\n```");
    }

    #[test]
    fn tables_render_with_alignments_and_uneven_rows() {
        render_ok("| L | C | R |\n|:--|:--:|--:|\n| a | b | c |");
        render_ok("| a | b |\n|---|---|\n| 1 | 2 | 3 |");
        render_ok("| only |\n|---|\n| x |");
    }
}
