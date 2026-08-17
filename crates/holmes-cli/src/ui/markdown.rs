//! Markdown rendering to ratatui lines, plus the streaming "checkpoint freeze" state
//! machine that backs live assistant output.
//!
//! Two halves:
//!
//! - [`render_markdown`] — a one-shot pulldown-cmark event walker producing styled
//!   `Line`s (headings, emphasis, inline code, fenced code blocks with syntect
//!   highlighting, nested lists, tables, blockquotes, links, rules). Code and table
//!   rows are *clipped* to the render width (they must never wrap — `fit_lines` would
//!   break their alignment); everything else is emitted unwrapped and wrapped later by
//!   `push_scrollback` / the tail renderer via `ui::wrap::fit_lines`.
//! - [`MarkdownStream`] — accumulates streamed `TextDelta` text into one `source`
//!   string. Scrollback is immutable once committed, so a block may only be committed
//!   when later input can no longer change how it parses: [`freeze_point`] finds the
//!   last *top-level* block that is closed AND followed by a blank line (the blank line
//!   is what guarantees e.g. a paragraph can't turn into a setext heading or lazy
//!   continuation). Frozen prefixes render deterministically, so committing
//!   `render(prefix)[committed..]` is exactly a prefix of the final render — known
//!   exception: reference-style link definitions at the END of the document can restyle
//!   already-committed text; accepted (LLM output overwhelmingly uses inline links).
//!
//! Every public entry point wraps the parser in `catch_unwind`: on any panic inside
//! pulldown-cmark/syntect the caller gets a plain-text fallback instead of a dead TUI.

use std::panic::{catch_unwind, AssertUnwindSafe};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ui::highlight::highlight_code;
use crate::ui::theme::{glyphs, theme};
use crate::ui::wrap::fit_lines;

/// pulldown-cmark extensions the renderer supports.
fn md_options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH
}

/// Accent gutter drawn at the left edge of fenced code blocks.
const CODE_GUTTER: &str = "▎ ";
/// Hard cap on table column width (display columns) before shrinking-to-fit kicks in.
const MAX_COL_W: usize = 40;
/// Floor for table column shrinking — below this we just let the row clip.
const MIN_COL_W: usize = 6;
/// Cap on the live-region tail height (the caller also clamps to half the terminal).
pub const MAX_TAIL_ROWS: usize = 20;

/// The assistant-message header prepended to the first rendered line of each block.
pub const HOLMES_PREFIX: &str = "Holmes  ";

fn prefix_span() -> Span<'static> {
    Span::styled(
        HOLMES_PREFIX,
        Style::default()
            .fg(theme().accent)
            .add_modifier(Modifier::BOLD),
    )
}

/// Prepend the `Holmes  ` header to the first line. Continuation lines get NO blanket
/// indent: markdown structures (nested lists, tables, code gutters) carry their own
/// indentation, and an 8-column shift would misalign tables and waste width — the bold
/// accent header is enough of a visual anchor. (The pre-markdown plain-text streamer
/// indented continuations by 8; this intentionally replaces that look.)
fn prepend_prefix(lines: &mut [Line<'static>]) {
    if let Some(first) = lines.first_mut() {
        first.spans.insert(0, prefix_span());
    }
}

/// Render a complete, non-streamed assistant message (MessageToUser / FinalAnswer /
/// NeedsUserInput / PlanUpdate): full markdown render + the `Holmes  ` header.
pub fn assistant_block(source: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = render_markdown(source, width);
    prepend_prefix(&mut lines);
    lines
}

/// Render markdown to styled lines. Code/table rows are clipped to `width`; prose is
/// emitted unwrapped (callers run `fit_lines` when they know the target width).
pub fn render_markdown(source: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    catch_unwind(AssertUnwindSafe(|| render_inner(source, width)))
        .unwrap_or_else(|_| plain_fallback(source))
}

/// Conservative fallback: the raw text, one line per source line, no styling.
fn plain_fallback(source: &str) -> Vec<Line<'static>> {
    source
        .lines()
        .map(|l| Line::from(Span::raw(l.to_string())))
        .collect()
}

/// Byte offset of the last safe freeze point in `source`: the end of the last
/// top-level block that is closed and followed by a blank line. `source[..point]` is
/// safe to render + commit to immutable scrollback. Returns 0 when nothing is safe.
pub fn freeze_point(source: &str) -> usize {
    catch_unwind(AssertUnwindSafe(|| freeze_point_inner(source))).unwrap_or(0)
}

fn freeze_point_inner(source: &str) -> usize {
    let mut depth = 0usize;
    let mut point = 0usize;
    for (ev, range) in Parser::new_ext(source, md_options()).into_offset_iter() {
        match ev {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(p) = blank_separated_end(source, range.end) {
                        point = p;
                    }
                }
            }
            _ => {}
        }
    }
    point
}

/// If the top-level block whose End event reports `pos` is followed by a blank line,
/// return the freeze point: the end of the block's content, trailing whitespace
/// trimmed (pulldown-cmark End ranges may or may not include the line's trailing
/// newline, so normalize by scanning the whitespace run from BOTH sides). The blank
/// line = at least two newlines between the block content and the next content.
fn blank_separated_end(source: &str, pos: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n');
    let mut content_end = pos.min(bytes.len());
    while content_end > 0 && is_ws(bytes[content_end - 1]) {
        content_end -= 1;
    }
    let mut newlines = 0;
    let mut i = content_end;
    while i < bytes.len() && is_ws(bytes[i]) {
        if bytes[i] == b'\n' {
            newlines += 1;
        }
        i += 1;
    }
    (newlines >= 2).then_some(content_end)
}

// ── one-shot renderer ──

fn render_inner(source: &str, width: usize) -> Vec<Line<'static>> {
    let mut r = Renderer::new(width);
    for ev in Parser::new_ext(source, md_options()) {
        r.event(ev);
    }
    r.finish()
}

struct ItemCtx {
    /// First-line marker, e.g. `"- "` or `"1. "`.
    marker: String,
    /// Continuation indent (spaces matching the marker's column width).
    cont: String,
    /// Lines already emitted inside this item.
    lines: usize,
}

#[derive(Default)]
struct TableCtx {
    /// Completed rows: (is_header, cells).
    rows: Vec<(bool, Vec<Vec<Span<'static>>>)>,
    /// Cells of the row currently being collected.
    current: Vec<Vec<Span<'static>>>,
    /// Spans of the cell currently being collected.
    cell: Vec<Span<'static>>,
    in_head: bool,
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    /// Spans of the line being built.
    cur: Vec<Span<'static>>,
    /// Whether the current line has started (its quote/list prefix is emitted).
    cur_open: bool,
    quote_depth: usize,
    items: Vec<ItemCtx>,
    /// Ordered-list counters; `None` = bullet list.
    lists: Vec<Option<u64>>,
    bold: bool,
    italic: bool,
    strike: bool,
    heading: Option<HeadingLevel>,
    /// (language token, raw body) while inside a fenced code block.
    code_block: Option<(String, String)>,
    table: Option<TableCtx>,
    /// Open links: (span index in the target buffer, url, in_table).
    links: Vec<(usize, String, bool)>,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Renderer {
            width,
            out: Vec::new(),
            cur: Vec::new(),
            cur_open: false,
            quote_depth: 0,
            items: Vec::new(),
            lists: Vec::new(),
            bold: false,
            italic: false,
            strike: false,
            heading: None,
            code_block: None,
            table: None,
            links: Vec::new(),
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                // Inline code: distinct fg so it reads as code even without a bg patch.
                self.push_inline(&t, Style::default().fg(theme().accent_busy));
            }
            // A source newline is a VISUAL line break in the terminal renderer (not
            // the CommonMark space): it keeps blockquote/list structure intact and
            // matches how the pre-markdown line streamer displayed assistant text.
            // Word wrapping happens later per line via `fit_lines`. Inside table cells
            // a newline collapses to a space instead.
            Event::SoftBreak => {
                if let Some(t) = &mut self.table {
                    t.cell.push(Span::raw(" "));
                } else {
                    self.flush_line();
                }
            }
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.block_start();
                self.out.push(Line::from(Span::styled(
                    "─".repeat(self.width),
                    Style::default().fg(theme().text_faint),
                )));
            }
            // Raw HTML passes through deemphasized rather than vanishing.
            Event::Html(t) | Event::InlineHtml(t) => {
                self.push_inline(&t, Style::default().fg(theme().text_faint))
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.block_start(),
            Tag::Heading { level, .. } => {
                self.block_start();
                self.heading = Some(level);
            }
            Tag::CodeBlock(kind) => {
                self.block_start();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code_block = Some((lang, String::new()));
            }
            Tag::Table(_) => {
                self.block_start();
                self.table = Some(TableCtx::default());
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                    t.current = Vec::new();
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.current = Vec::new();
                }
            }
            Tag::TableCell => {
                if let Some(t) = &mut self.table {
                    t.cell = Vec::new();
                }
            }
            Tag::BlockQuote(..) => {
                if self.quote_depth == 0 {
                    self.block_start();
                }
                self.quote_depth += 1;
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block_start();
                } else if self.cur_open {
                    // Nested list: the parent item's text line ends here.
                    self.flush_line();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                if self.cur_open {
                    self.flush_line();
                }
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "- ".to_string(),
                };
                let cont = " ".repeat(marker.len());
                self.items.push(ItemCtx {
                    marker,
                    cont,
                    lines: 0,
                });
            }
            Tag::Strong => self.bold = true,
            Tag::Emphasis => self.italic = true,
            Tag::Strikethrough => self.strike = true,
            Tag::Link { dest_url, .. } => {
                let (idx, in_table) = match &self.table {
                    Some(t) => (t.cell.len(), true),
                    None => (self.cur.len(), false),
                };
                self.links.push((idx, dest_url.to_string(), in_table));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_line(),
            TagEnd::Heading(_) => {
                self.heading = None;
                self.flush_line();
            }
            TagEnd::CodeBlock => self.finish_code_block(),
            TagEnd::Table => self.finish_table(),
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.current);
                    t.rows.push((t.in_head, row));
                    t.in_head = false;
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    let cell = std::mem::take(&mut t.cell);
                    t.current.push(cell);
                }
            }
            TagEnd::BlockQuote(..) => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush_line();
                self.items.pop();
            }
            TagEnd::Strong => self.bold = false,
            TagEnd::Emphasis => self.italic = false,
            TagEnd::Strikethrough => self.strike = false,
            TagEnd::Link => self.finish_link(),
            _ => {}
        }
    }

    /// Flush any open line and insert a blank separator between top-level blocks.
    fn block_start(&mut self) {
        if self.cur_open {
            self.flush_line();
        }
        if self.out.last().is_some_and(|l| !l.spans.is_empty()) {
            self.out.push(Line::default());
        }
    }

    /// Begin the current line: quote bars, then list markers/indents. Only the
    /// innermost item contributes its `- `/`1. ` marker (and only on its first line);
    /// outer items contribute their continuation indent.
    fn open_line(&mut self) {
        if self.cur_open {
            return;
        }
        self.cur_open = true;
        let bar = format!("{} ", glyphs().blockquote);
        for _ in 0..self.quote_depth {
            self.cur.push(Span::styled(
                bar.clone(),
                Style::default().fg(theme().text_muted),
            ));
        }
        let last = self.items.len().saturating_sub(1);
        for (i, it) in self.items.iter_mut().enumerate() {
            let s = if i == last && it.lines == 0 {
                it.marker.clone()
            } else {
                it.cont.clone()
            };
            self.cur.push(Span::raw(s));
        }
    }

    fn flush_line(&mut self) {
        self.cur_open = false;
        if self.cur.is_empty() {
            return;
        }
        self.out.push(Line::from(std::mem::take(&mut self.cur)));
        for it in &mut self.items {
            it.lines += 1;
        }
    }

    /// Push styled inline text into the right sink: code-block body, table cell, or
    /// the current line.
    fn push_inline(&mut self, text: &str, style: Style) {
        if self.code_block.is_some() {
            // Code-block bodies arrive via `text`; a stray inline event while a code
            // block is open is appended raw rather than dropped.
            if let Some((_, buf)) = &mut self.code_block {
                buf.push_str(text);
            }
            return;
        }
        if let Some(t) = &mut self.table {
            t.cell.push(Span::styled(text.to_string(), style));
            return;
        }
        self.open_line();
        self.cur.push(Span::styled(text.to_string(), style));
    }

    fn text(&mut self, t: &str) {
        let style = self.inline_style();
        self.push_inline(t, style);
    }

    fn inline_style(&self) -> Style {
        if let Some(level) = self.heading {
            let mut st = Style::default()
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD);
            if level == HeadingLevel::H1 {
                st = st.add_modifier(Modifier::UNDERLINED);
            }
            return st;
        }
        let mut m = Modifier::empty();
        if self.bold {
            m |= Modifier::BOLD;
        }
        if self.italic {
            m |= Modifier::ITALIC;
        }
        if self.strike {
            m |= Modifier::CROSSED_OUT;
        }
        Style::default().add_modifier(m)
    }

    /// Close a link: recolor its text accent and append the URL muted (no OSC 8 —
    /// inline viewports don't participate in cell diffing, and scrollback copy-paste
    /// benefits from the URL being literal text).
    fn finish_link(&mut self) {
        let Some((idx, url, in_table)) = self.links.pop() else {
            return;
        };
        let buf: &mut Vec<Span<'static>> = match (in_table, &mut self.table) {
            (true, Some(t)) => &mut t.cell,
            _ => &mut self.cur,
        };
        let text: String = buf[idx..].iter().map(|s| s.content.as_ref()).collect();
        for span in &mut buf[idx..] {
            span.style = span.style.fg(theme().accent);
        }
        if text != url {
            buf.push(Span::styled(
                format!(" ({url})"),
                Style::default().fg(theme().text_faint),
            ));
        }
    }

    fn finish_code_block(&mut self) {
        let Some((lang, body)) = self.code_block.take() else {
            return;
        };
        let lines: Vec<&str> = body.lines().collect();
        let highlighted = if lang.is_empty() {
            None
        } else {
            highlight_code(&lang, &lines)
        };
        let gutter = || Span::styled(CODE_GUTTER, Style::default().fg(theme().accent));
        match highlighted {
            Some(token_lines) => {
                for tokens in token_lines {
                    let mut spans = vec![gutter()];
                    spans.extend(tokens.into_iter().map(|(st, s)| Span::styled(s, st)));
                    self.out.push(clip_line(Line::from(spans), self.width));
                }
            }
            None => {
                for l in lines {
                    let line = Line::from(vec![
                        gutter(),
                        Span::styled(l.to_string(), Style::default().fg(theme().text_muted)),
                    ]);
                    self.out.push(clip_line(line, self.width));
                }
            }
        }
    }

    fn finish_table(&mut self) {
        let Some(t) = self.table.take() else {
            return;
        };
        self.out.extend(render_table(&t.rows, self.width));
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        std::mem::take(&mut self.out)
    }
}

/// Space-aligned table: bold header row, a `─` separator, two-space column gaps.
/// Column widths come from content display width (capped at `MAX_COL_W`, then shrunk
/// widest-first until the row fits `width`); over-wide cells truncate with `…`.
/// No `│` box borders — they cost two columns per edge and fight with narrow terminals.
fn render_table(rows: &[(bool, Vec<Vec<Span<'static>>>)], width: usize) -> Vec<Line<'static>> {
    let cols = rows.iter().map(|(_, r)| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }
    let mut col_w = vec![0usize; cols];
    for (_, row) in rows {
        for (c, cell) in row.iter().enumerate() {
            col_w[c] = col_w[c].max(spans_width(cell));
        }
    }
    for w in &mut col_w {
        *w = (*w).min(MAX_COL_W);
    }
    let gaps = 2 * cols.saturating_sub(1);
    while col_w.iter().sum::<usize>() + gaps > width {
        let Some((i, _)) = col_w.iter().enumerate().max_by_key(|(_, w)| *w) else {
            break;
        };
        if col_w[i] <= MIN_COL_W {
            break;
        }
        col_w[i] -= 1;
    }
    let mut out = Vec::new();
    for (is_head, row) in rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (c, w) in col_w.iter().enumerate() {
            if c > 0 {
                spans.push(Span::raw("  "));
            }
            let (mut cell, used) = clip_spans(row.get(c).map(|v| v.as_slice()).unwrap_or(&[]), *w);
            spans.append(&mut cell);
            if used < *w {
                spans.push(Span::raw(" ".repeat(*w - used)));
            }
        }
        if *is_head {
            for s in &mut spans {
                s.style = s.style.add_modifier(Modifier::BOLD);
            }
        }
        out.push(Line::from(spans));
        if *is_head {
            let rule = col_w
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("  ");
            out.push(Line::from(Span::styled(
                rule,
                Style::default().fg(theme().text_faint),
            )));
        }
    }
    out
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Truncate spans to `max` display columns, appending a faint `…` when anything was
/// cut. Returns the spans and the width actually used.
fn clip_spans(spans: &[Span<'static>], max: usize) -> (Vec<Span<'static>>, usize) {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for s in spans {
        let w = s.content.width();
        if used + w <= max {
            out.push(s.clone());
            used += w;
            continue;
        }
        let lim = max.saturating_sub(used + 1);
        let mut taken = String::new();
        let mut tw = 0usize;
        for ch in s.content.chars() {
            let cw = ch.width().unwrap_or(0);
            if tw + cw > lim {
                break;
            }
            taken.push(ch);
            tw += cw;
        }
        if !taken.is_empty() {
            out.push(Span::styled(taken, s.style));
        }
        out.push(Span::styled(
            glyphs().ellipsis.to_string(),
            Style::default().fg(theme().text_faint),
        ));
        return (out, max);
    }
    (out, used)
}

/// Clip a whole line (code/table rows must never wrap — `fit_lines` would destroy
/// their alignment, so they are truncated with an ellipsis instead).
fn clip_line(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    Line::from(clip_spans(&line.spans, width).0)
}

// ── streaming state ──

/// The result of one [`MarkdownStream::refresh`] pass.
pub struct StreamRefresh {
    /// Newly frozen lines to commit to scrollback (already prefix-styled).
    pub commit: Vec<Line<'static>>,
    /// The current open tail, wrapped to the render width and capped — render this in
    /// the live region above the input box.
    pub tail: Vec<Line<'static>>,
}

/// Accumulates streamed assistant text and decides what may be committed to immutable
/// scrollback (closed top-level blocks followed by a blank line) vs. what stays in the
/// redrawable live-region tail. Pure — no terminal access — so the whole freeze/tail
/// state machine is unit-testable; the caller applies `commit` via `push_scrollback`
/// and draws `tail`.
#[derive(Default)]
pub struct MarkdownStream {
    source: String,
    /// Byte offset: `source[..frozen_upto]` is fully committed.
    frozen_upto: usize,
    /// Rendered-line count committed so far (index into prefix renders).
    committed: usize,
    /// Whether the `Holmes  ` header was emitted for this block.
    prefixed: bool,
    /// Any delta received for the current block (drives MessageToUser/FinalAnswer
    /// dedup: a full-text block that only repeats streamed text is suppressed).
    streamed: bool,
    /// A `\n` arrived since the last refresh — the refresh gate (rendering only
    /// advances on line boundaries, so mid-word deltas never trigger a re-render).
    pending_newline: bool,
}

impl MarkdownStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_delta(&mut self, text: &str) {
        if text.contains('\n') {
            self.pending_newline = true;
        }
        self.streamed = true;
        self.source.push_str(text);
    }

    pub fn has_pending_newline(&self) -> bool {
        self.pending_newline
    }

    pub fn streamed(&self) -> bool {
        self.streamed
    }

    /// Mark the trailing full-text block consumed (it was suppressed as a duplicate).
    pub fn clear_streamed(&mut self) {
        self.streamed = false;
    }

    /// Re-render the accumulated source: freeze newly closed blocks into `commit`,
    /// and return the open tail (wrapped to `width`, capped to the LAST `tail_cap`
    /// rows — the tail scrolls, newest content wins). Rendering the prefix again and
    /// slicing off `committed` lines is what keeps scrollback consistent with the
    /// final full render.
    pub fn refresh(&mut self, width: usize, tail_cap: usize) -> StreamRefresh {
        self.pending_newline = false;
        let mut commit = Vec::new();
        let fp = freeze_point(&self.source);
        if fp > self.frozen_upto {
            let prefix_lines = render_markdown(&self.source[..fp], width);
            if prefix_lines.len() > self.committed {
                commit = prefix_lines[self.committed..].to_vec();
                self.apply_prefix(&mut commit);
                self.committed = prefix_lines.len();
            }
            self.frozen_upto = fp;
        }
        let mut tail = render_markdown(&self.source[self.frozen_upto..], width);
        if !self.prefixed {
            // Display-only: when these lines later freeze they render from source and
            // get the header via `apply_prefix`, so there is no double-prefixing.
            prepend_prefix(&mut tail);
        }
        let mut tail = fit_lines(tail, width);
        if tail.len() > tail_cap {
            tail = tail.split_off(tail.len() - tail_cap);
        }
        StreamRefresh { commit, tail }
    }

    /// Treat the accumulated source as complete (the LLM call ended — any open block
    /// can be safely closed by the parser's EOF handling) and return every line not
    /// yet committed. Resets the block state so the next delta starts a fresh
    /// `Holmes  `-prefixed block.
    pub fn finalize(&mut self, width: usize) -> Vec<Line<'static>> {
        let mut rest = if self.source.is_empty() {
            Vec::new()
        } else {
            let all = render_markdown(&self.source, width);
            if all.len() > self.committed {
                all[self.committed..].to_vec()
            } else {
                Vec::new()
            }
        };
        self.apply_prefix(&mut rest);
        self.source.clear();
        self.frozen_upto = 0;
        self.committed = 0;
        self.prefixed = false;
        self.pending_newline = false;
        rest
    }

    fn apply_prefix(&mut self, lines: &mut Vec<Line<'static>>) {
        if !self.prefixed && !lines.is_empty() {
            prepend_prefix(lines);
            self.prefixed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    // ── syntax elements ──

    #[test]
    fn heading_is_bold_accent_without_hashes() {
        let out = render_markdown("# Title\n\ntext", 80);
        assert_eq!(plain(&out)[0], "Title");
        let h = &out[0].spans[0];
        assert_eq!(h.style.fg, Some(theme().accent));
        assert!(h.style.add_modifier.contains(Modifier::BOLD));
        assert!(h.style.add_modifier.contains(Modifier::UNDERLINED));
        let h2 = render_markdown("## Sub", 80);
        assert!(!h2[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn inline_styles_map_to_modifiers() {
        let out = render_markdown("**b** *i* ~~s~~ `c`", 80);
        let find = |needle: &str| {
            out[0]
                .spans
                .iter()
                .find(|s| s.content.contains(needle))
                .unwrap_or_else(|| panic!("span {needle}"))
                .clone()
        };
        assert!(find("b").style.add_modifier.contains(Modifier::BOLD));
        assert!(find("i").style.add_modifier.contains(Modifier::ITALIC));
        assert!(find("s").style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(find("c").style.fg, Some(theme().accent_busy));
    }

    #[test]
    fn code_block_gets_gutter_and_highlight() {
        let out = render_markdown("```rust\nfn main() {}\n```", 80);
        assert_eq!(out.len(), 1);
        assert!(plain(&out)[0].starts_with(CODE_GUTTER));
        assert!(plain(&out)[0].contains("fn main() {}"));
        // Tests run with the TrueColor fallback → syntect highlighting is active and
        // tokens carry RGB colors (not the flat muted fallback).
        assert!(out[0]
            .spans
            .iter()
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _)))));
    }

    #[test]
    fn code_block_unknown_language_falls_back_to_muted_plain() {
        let out = render_markdown("```zzzqqq\nhello code\n```", 80);
        assert_eq!(plain(&out), vec![format!("{CODE_GUTTER}hello code")]);
        assert_eq!(out[0].spans[1].style.fg, Some(theme().text_muted));
    }

    #[test]
    fn code_lines_clip_instead_of_wrap() {
        let long = "x".repeat(50);
        let out = render_markdown(&format!("```\n{long}\n```"), 20);
        assert_eq!(out.len(), 1, "code line must not wrap");
        assert_eq!(out[0].width(), 20);
        assert!(plain(&out)[0].ends_with(glyphs().ellipsis));
    }

    #[test]
    fn nested_and_ordered_lists() {
        let out = render_markdown("- a\n  - b\n  - c\n\n1. x\n2. y", 80);
        let t = plain(&out);
        assert_eq!(t[0], "- a");
        assert_eq!(t[1], "  - b");
        assert_eq!(t[2], "  - c");
        assert_eq!(t[3], "");
        assert_eq!(t[4], "1. x");
        assert_eq!(t[5], "2. y");
    }

    #[test]
    fn list_continuation_lines_indent() {
        let out = render_markdown("- a\n  continued", 80);
        let t = plain(&out);
        assert_eq!(t[0], "- a");
        assert_eq!(t[1], "  continued");
    }

    #[test]
    fn table_is_space_aligned_with_bold_header() {
        let out = render_markdown(
            "| name | age |\n| --- | --- |\n| al | 3 |\n| bob | 22 |",
            80,
        );
        let t = plain(&out);
        assert_eq!(t.len(), 4);
        assert_eq!(t[0], "name  age");
        assert!(
            t[1].chars().all(|c| c == '─' || c == ' '),
            "separator: {:?}",
            t[1]
        );
        assert_eq!(t[2], "al    3  ");
        assert_eq!(t[3], "bob   22 ");
        assert!(out[0]
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn table_shrinks_and_truncates_to_width() {
        let long = "abcdefghij".repeat(6);
        let md = format!("| c1 | c2 |\n| --- | --- |\n| {long} | v |");
        let out = render_markdown(&md, 30);
        for line in &out {
            assert!(line.width() <= 30, "row fits: {:?}", line_text(line));
        }
        assert!(plain(&out).iter().any(|l| l.contains(glyphs().ellipsis)));
    }

    #[test]
    fn blockquote_lines_get_bar_prefix() {
        let out = render_markdown("> one\n> two", 80);
        let t = plain(&out);
        let bar = format!("{} ", glyphs().blockquote);
        assert_eq!(t, vec![format!("{bar}one"), format!("{bar}two")]);
        assert_eq!(out[0].spans[0].style.fg, Some(theme().text_muted));
    }

    #[test]
    fn link_renders_accent_text_plus_muted_url() {
        let out = render_markdown("see [docs](https://x.dev) now", 80);
        let text = plain(&out)[0].clone();
        assert_eq!(text, "see docs (https://x.dev) now");
        let link = out[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "docs")
            .unwrap();
        assert_eq!(link.style.fg, Some(theme().accent));
        let url = out[0]
            .spans
            .iter()
            .find(|s| s.content.contains("https://x.dev"))
            .unwrap();
        assert_eq!(url.style.fg, Some(theme().text_faint));
    }

    #[test]
    fn rule_is_full_width_muted_line() {
        let out = render_markdown("a\n\n---\n\nb", 40);
        let t = plain(&out);
        assert!(t.iter().any(|l| *l == "─".repeat(40)));
    }

    #[test]
    fn blocks_are_blank_separated() {
        let out = render_markdown("p1\n\np2", 80);
        assert_eq!(plain(&out), vec!["p1", "", "p2"]);
    }

    // ── freeze points ──

    #[test]
    fn paragraph_without_blank_line_does_not_freeze() {
        assert_eq!(freeze_point("hello"), 0);
        assert_eq!(freeze_point("hello\nworld"), 0);
        assert_eq!(freeze_point("hello\n"), 0);
    }

    #[test]
    fn paragraph_freezes_after_blank_line() {
        assert_eq!(freeze_point("hello\n\nworld"), 5);
        assert_eq!(freeze_point("hello\n\n"), 5);
        // Two closed paragraphs: the freeze point advances to the second one.
        let md = "a\n\nb\n\nc";
        let p = freeze_point(md);
        assert_eq!(&md[..p], "a\n\nb");
    }

    #[test]
    fn code_block_freezes_only_when_closed() {
        let md = "```rust\nx = 1;\n```\n\nnext";
        let p = freeze_point(md);
        assert_eq!(&md[..p], "```rust\nx = 1;\n```");
        // Still streaming inside the fence: nothing is safe.
        assert_eq!(freeze_point("```rust\nx = 1;"), 0);
        assert_eq!(freeze_point("```rust\nx = 1;\n"), 0);
    }

    #[test]
    fn table_freezes_when_closed_and_blank_follows() {
        let md = "| a |\n| --- |\n| 1 |\n\nafter";
        let p = freeze_point(md);
        assert_eq!(&md[..p], "| a |\n| --- |\n| 1 |");
    }

    #[test]
    fn list_freezes_as_one_top_level_block() {
        let md = "- a\n- b\n\nnext";
        let p = freeze_point(md);
        assert_eq!(&md[..p], "- a\n- b");
    }

    // ── streaming ──

    #[test]
    fn streamed_commits_equal_one_shot_render() {
        let doc = "# Report\n\nFirst **para** with `code`.\n\n- one\n- two\n\n```rust\nfn x() {}\n```\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\nFinal words here.\n";
        // Feed in awkward chunks; refresh whenever a newline landed.
        let mut s = MarkdownStream::new();
        let mut committed: Vec<Line<'static>> = Vec::new();
        let mut i = 0;
        for chunk in doc.as_bytes().chunks(7) {
            let text = std::str::from_utf8(chunk).unwrap();
            s.push_delta(text);
            if s.has_pending_newline() {
                let r = s.refresh(60, MAX_TAIL_ROWS);
                committed.extend(r.commit);
                i += 1;
            }
        }
        assert!(i > 0, "refresh happened");
        committed.extend(s.finalize(60));
        assert_eq!(committed, assistant_block(doc, 60));
    }

    #[test]
    fn tail_shows_unfrozen_suffix_with_prefix_until_committed() {
        let mut s = MarkdownStream::new();
        s.push_delta("done para\n\nopen para");
        let r = s.refresh(80, MAX_TAIL_ROWS);
        // "done para" froze (closed + blank line) and carries the header.
        assert_eq!(plain(&r.commit), vec![format!("{HOLMES_PREFIX}done para")]);
        assert_eq!(plain(&r.tail), vec!["open para"]);
        // Next refresh: nothing new frozen, tail has no header (already committed).
        s.push_delta(" continues");
        let r = s.refresh(80, MAX_TAIL_ROWS);
        assert!(r.commit.is_empty());
        assert_eq!(plain(&r.tail), vec!["open para continues"]);
    }

    #[test]
    fn tail_caps_to_last_n_rows() {
        let mut s = MarkdownStream::new();
        // One open (unfreezable) paragraph with many wrapped lines.
        let mut text = String::new();
        for i in 0..30 {
            text.push_str(&format!("word{i} "));
        }
        s.push_delta(&text);
        let r = s.refresh(20, 5);
        assert!(r.commit.is_empty());
        assert_eq!(r.tail.len(), 5);
        // Kept rows are the LAST ones (the tail scrolls): they end with the last word.
        let last = plain(&r.tail);
        assert!(last[4].contains("word29"));
    }

    #[test]
    fn finalize_closes_open_blocks_and_resets_prefix() {
        let mut s = MarkdownStream::new();
        s.push_delta("```rust\nfn x() {}\n");
        let rest = s.finalize(80);
        // Unclosed fence is closed at EOF and rendered as a code block.
        assert_eq!(rest.len(), 1);
        assert!(plain(&rest)[0].starts_with(&format!("{HOLMES_PREFIX}{CODE_GUTTER}")));
        // A new block gets its own prefix again.
        s.push_delta("next block");
        let rest = s.finalize(80);
        assert_eq!(plain(&rest), vec![format!("{HOLMES_PREFIX}next block")]);
    }

    #[test]
    fn finalize_without_streamed_content_is_empty() {
        let mut s = MarkdownStream::new();
        assert!(s.finalize(80).is_empty());
    }

    #[test]
    fn dedup_flags_track_streamed_blocks() {
        let mut s = MarkdownStream::new();
        assert!(!s.streamed());
        s.push_delta("x");
        assert!(s.streamed());
        s.clear_streamed();
        assert!(!s.streamed());
    }

    #[test]
    fn assistant_block_prefixes_first_line_only() {
        let out = assistant_block("hello\n\nworld", 80);
        let t = plain(&out);
        assert_eq!(t[0], format!("{HOLMES_PREFIX}hello"));
        assert_eq!(t[1], "");
        assert_eq!(t[2], "world", "no indent/re-prefix on later lines");
    }
}
