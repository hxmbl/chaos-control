//! Markdown rendering for conversation messages.
//!
//! Renders CommonMark-ish markdown to gpui elements for display in the chat.
//! The composer remains plain-text source editing; this renderer is used only
//! for message display so sent content is shown with formatting while the
//! stored payload stays unchanged.
//!
//! Mention highlighting (known `@members`/broadcasts) is applied to rendered
//! text so it stays consistent with plain-text rendering.

use gpui::{
    AnyElement, FontStyle, FontWeight, Hsla, SharedString, StrikethroughStyle, StyledText, TextRun,
    UnderlineStyle, div, prelude::*, px,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ui::{
    TypeStyleExt,
    tokens::{radius, space, text},
};

use crate::mention_highlight::{self, KnownMentions};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: bool,
}

#[derive(Clone)]
struct Span {
    text: String,
    style: InlineStyle,
}

pub fn render_message_markdown(
    source: &str,
    known: &KnownMentions,
    text_color: Hsla,
    base_style: &gpui::TextStyle,
    theme: &ui::Theme,
) -> AnyElement {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(source, opts);
    let mut renderer = Renderer::new(known, text_color, theme, base_style);
    renderer.run(parser);
    renderer.finish().into_any_element()
}

#[derive(Clone)]
struct ListState {
    ordered: bool,
    start: u64,
}

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
    pending_heading: Option<HeadingLevel>,
    list_stack: Vec<(ListState, Vec<Vec<AnyElement>>)>,
    item_block_stack: Vec<Vec<AnyElement>>,
    in_list_item: usize,
}

impl<'a> Renderer<'a> {
    fn new(
        known: &'a KnownMentions,
        text_color: Hsla,
        theme: &'a ui::Theme,
        base_style: &'a gpui::TextStyle,
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
            pending_heading: None,
            list_stack: Vec::new(),
            item_block_stack: Vec::new(),
            in_list_item: 0,
        }
    }

    fn current_style(&self) -> InlineStyle {
        *self.style_stack.last().unwrap_or(&InlineStyle::default())
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
        if let Some(last) = self.para_spans.last_mut() {
            if last.style == style {
                last.text.push_str(text);
                return;
            }
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
        self.push_block(element);
    }

    fn push_block(&mut self, element: AnyElement) {
        let element = self.wrap_blockquote(element);
        if self.in_list_item > 0 {
            if let Some(item) = self.item_block_stack.last_mut() {
                item.push(element);
                return;
            }
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
        let mut runs: Vec<TextRun> = Vec::new();
        let mut offset = 0usize;
        for span in spans {
            let start = offset;
            let end = offset + span.text.len();
            self.push_styled_runs(
                &full,
                base_style,
                start,
                end,
                span.style,
                &mention_spans,
                &mut runs,
            );
            offset = end;
        }
        let text_el = StyledText::new(SharedString::from(full)).with_runs(runs);
        let mut d = div()
            .w_full()
            .min_w_0()
            .whitespace_normal()
            .text_color(self.text_color);
        if let Some(level) = heading {
            let (ts, weight, color) = match level {
                HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3 => {
                    (text::BODY, text::SEMIBOLD, self.theme.ink)
                }
                _ => (text::BODY, text::MEDIUM, self.theme.ink),
            };
            d = d.type_style(ts).font_weight(weight).text_color(color);
        }
        d.child(text_el).into_any_element()
    }

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
                font.weight = FontWeight::SEMIBOLD;
            }
            if style.italic {
                font.style = FontStyle::Italic;
            }
            let mut color = if style.link {
                self.theme.accent
            } else {
                self.text_color
            };
            let mut bg = base.background_color;
            let mut underline = base.underline.clone();
            let mut strike = base.strikethrough.clone();
            if style.strike {
                strike = Some(StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color),
                });
            }
            if style.link {
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
                Event::SoftBreak | Event::HardBreak => self.push_text("\n"),
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
                    self.push_text(if checked { "[x] " } else { "[ ] " });
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
            Tag::CodeBlock(_) => {
                self.flush_paragraph();
                self.in_code_block = true;
                self.code_block_text.clear();
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
            Tag::Link { .. } => {
                let mut s = self.current_style();
                s.link = true;
                self.style_stack.push(s);
            }
            Tag::Image { .. } => {
                self.push_text("[image]");
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
                let t = self.theme;
                let text: SharedString = code.trim_end_matches('\n').to_string().into();
                let el = div()
                    .w_full()
                    .px(space::X2)
                    .py(space::X1_5)
                    .rounded(radius::SM)
                    .bg(t.sunken)
                    .text_color(t.ink)
                    .type_style(text::SMALL)
                    .whitespace_normal()
                    .child(text);
                self.push_block(el.into_any_element());
            }
            TagEnd::List(_) => {
                self.flush_paragraph();
                if let Some((state, items)) = self.list_stack.pop() {
                    let t = self.theme;
                    let mut col = div().flex().flex_col().gap(space::X0_5);
                    for (i, item_blocks) in items.into_iter().enumerate() {
                        let marker = if state.ordered {
                            format!("{}.", state.start + i as u64)
                        } else {
                            "•".to_string()
                        };
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
                if let Some(item_blocks) = self.item_block_stack.pop() {
                    if let Some((_, items)) = self.list_stack.last_mut() {
                        items.push(item_blocks);
                    }
                }
                self.in_list_item = self.in_list_item.saturating_sub(1);
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                if self.style_stack.len() > 1 {
                    self.style_stack.pop();
                }
            }
            TagEnd::Image => {}
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
        let _ = render_message_markdown(source, &known, theme.ink_soft, &base_style(), &theme);
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
}
