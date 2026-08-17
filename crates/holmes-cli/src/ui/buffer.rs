//! Segment-based input buffer for the inline prompt.
//!
//! The prompt used to be a single `String`. Large pastes broke that model: a 200-line
//! paste sprayed raw newlines into a single-line input box. Grok's answer is atomic
//! *chips*: a big paste collapses into one `[Pasted: N lines]` placeholder that the
//! caret skips over and Backspace deletes whole, while the real content rides along
//! invisibly and is only expanded on submit. This module ports that mechanism.
//!
//! Model: the buffer is a `Vec<Segment>` (`Text` / `Paste` chip). All editing works in
//! **display coordinates** — the byte offset into the rendered text stream where a chip
//! contributes its placeholder label, not its content. The caret therefore never sits
//! inside a chip: every cursor-aware op treats a chip as one atomic unit.

/// A paste this many lines or longer collapses into a chip (grok's threshold).
pub const CHIP_MIN_LINES: usize = 4;
/// …or anything larger than this many bytes, even with few lines (minified blobs).
pub const CHIP_MIN_BYTES: usize = 10 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    /// An atomic paste chip. `lines` is what the placeholder shows; `content` is the
    /// full pasted text (newlines intact) that only reappears on submit/expand.
    Paste {
        lines: usize,
        content: String,
    },
}

impl Segment {
    fn display_len(&self) -> usize {
        match self {
            Segment::Text(t) => t.len(),
            Segment::Paste { lines, .. } => chip_label(*lines).len(),
        }
    }
}

/// The placeholder a chip renders as in the input box.
pub fn chip_label(lines: usize) -> String {
    format!("[Pasted: {lines} lines]")
}

/// Display-stream layout: the rendered string plus where each chip sits in it. Rebuilt
/// per edit — the prompt is short, so O(n) layout on every keystroke is fine and keeps
/// the segment model free of incremental-cache bugs.
struct Layout {
    display: String,
    /// Byte offset where each segment starts in `display` (parallel to `segments`).
    seg_starts: Vec<usize>,
    /// `(start, end, segment_index)` for every chip, in display coordinates.
    chips: Vec<(usize, usize, usize)>,
}

#[derive(Default)]
pub struct InputBuffer {
    segments: Vec<Segment>,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A plain-text buffer (what the old `String` model held).
    pub fn from_text(text: &str) -> Self {
        let mut buf = Self::new();
        buf.set_text(text);
        buf
    }

    fn layout(&self) -> Layout {
        let mut display = String::new();
        let mut seg_starts = Vec::with_capacity(self.segments.len());
        let mut chips = Vec::new();
        for (i, seg) in self.segments.iter().enumerate() {
            seg_starts.push(display.len());
            match seg {
                Segment::Text(t) => display.push_str(t),
                Segment::Paste { lines, .. } => {
                    let start = display.len();
                    display.push_str(&chip_label(*lines));
                    chips.push((start, display.len(), i));
                }
            }
        }
        Layout {
            display,
            seg_starts,
            chips,
        }
    }

    /// The rendered text stream (chips shown as placeholders). This is what the input
    /// box draws and what the caret offset indexes into.
    pub fn display(&self) -> String {
        self.layout().display
    }

    pub fn display_len(&self) -> usize {
        self.segments.iter().map(Segment::display_len).sum()
    }

    /// Chip ranges `(start, end)` in display coordinates, for styled rendering.
    pub fn chip_ranges(&self) -> Vec<(usize, usize)> {
        self.layout()
            .chips
            .into_iter()
            .map(|(s, e, _)| (s, e))
            .collect()
    }

    /// The full text to send on submit: chips expand back to their pasted content.
    pub fn expanded(&self) -> String {
        let mut out = String::new();
        for seg in &self.segments {
            match seg {
                Segment::Text(t) => out.push_str(t),
                Segment::Paste { content, .. } => out.push_str(content),
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty() || self.display_len() == 0
    }

    pub fn clear(&mut self) {
        self.segments.clear();
    }

    /// Replace everything with plain text (history recall, slash-menu accept).
    pub fn set_text(&mut self, text: &str) {
        self.segments.clear();
        if !text.is_empty() {
            self.segments.push(Segment::Text(text.to_string()));
        }
    }

    /// Keep the segment list canonical: no empty texts, no adjacent texts (so caret
    /// boundaries and word ops behave predictably).
    fn normalize(&mut self) {
        let mut out: Vec<Segment> = Vec::with_capacity(self.segments.len());
        for seg in std::mem::take(&mut self.segments) {
            match (out.last_mut(), seg) {
                (_, Segment::Text(t)) if t.is_empty() => {}
                (Some(Segment::Text(prev)), Segment::Text(t)) => prev.push_str(&t),
                (_, seg) => out.push(seg),
            }
        }
        self.segments = out;
    }

    /// Insert plain text at a display offset. Splits/merges text segments as needed;
    /// at a chip boundary the text joins the neighboring text segment (never the chip).
    fn insert_text_at(&mut self, offset: usize, s: &str) {
        if s.is_empty() {
            return;
        }
        let layout = self.layout();
        for i in 0..self.segments.len() {
            let start = layout.seg_starts[i];
            let end = start + self.segments[i].display_len();
            match &mut self.segments[i] {
                Segment::Text(t) => {
                    if (start..=end).contains(&offset) {
                        // Cursor offsets are always char boundaries in display space,
                        // and text segments contribute verbatim — so `offset - start`
                        // is a char boundary inside `t`.
                        t.insert_str(offset - start, s);
                        return;
                    }
                }
                Segment::Paste { .. } => {
                    if offset == start {
                        // Typing right before a chip: join the preceding text if there
                        // is one, else open a fresh text segment before the chip.
                        if i > 0 {
                            if let Segment::Text(prev) = &mut self.segments[i - 1] {
                                prev.push_str(s);
                                return;
                            }
                        }
                        self.segments.insert(i, Segment::Text(s.to_string()));
                        return;
                    }
                    if offset == end {
                        if i + 1 < self.segments.len() {
                            if let Segment::Text(next) = &mut self.segments[i + 1] {
                                next.insert_str(0, s);
                                return;
                            }
                        }
                        self.segments.insert(i + 1, Segment::Text(s.to_string()));
                        return;
                    }
                }
            }
        }
        // Empty buffer or caret at the very end.
        self.segments.push(Segment::Text(s.to_string()));
    }

    /// Insert a segment (a paste chip) at a display offset, splitting a text segment
    /// when the caret sits inside one.
    fn insert_segment_at(&mut self, offset: usize, seg: Segment) {
        let layout = self.layout();
        for i in 0..self.segments.len() {
            let start = layout.seg_starts[i];
            let end = start + self.segments[i].display_len();
            if offset == start {
                self.segments.insert(i, seg);
                return;
            }
            if offset == end {
                self.segments.insert(i + 1, seg);
                return;
            }
            if let Segment::Text(t) = &mut self.segments[i] {
                if (start..end).contains(&offset) {
                    let tail = t.split_off(offset - start);
                    self.segments.insert(i + 1, seg);
                    self.segments.insert(i + 2, Segment::Text(tail));
                    return;
                }
            }
        }
        self.segments.push(seg);
    }

    /// Delete the display range `[start, end)`. Atomic chip rule: any chip the range
    /// *touches* is removed whole — a chip has no interior to partially delete.
    fn delete_range(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let layout = self.layout();
        let mut out: Vec<Segment> = Vec::with_capacity(self.segments.len());
        for (i, seg) in self.segments.iter().enumerate() {
            let s = layout.seg_starts[i];
            let e = s + seg.display_len();
            if e <= start || s >= end {
                out.push(seg.clone());
                continue;
            }
            match seg {
                // Touched by the range → the whole chip goes.
                Segment::Paste { .. } => {}
                Segment::Text(t) => {
                    let keep_head = &t[..(start.saturating_sub(s)).min(t.len())];
                    let keep_tail = &t[(end.saturating_sub(s)).min(t.len())..];
                    let kept = format!("{keep_head}{keep_tail}");
                    if !kept.is_empty() {
                        out.push(Segment::Text(kept));
                    }
                }
            }
        }
        self.segments = out;
        self.normalize();
    }

    // ── cursor-aware editing (cursor = byte offset into `display()`) ──

    pub fn insert_char(&mut self, cursor: &mut usize, c: char) {
        let mut buf = [0u8; 4];
        self.insert_text_at(*cursor, c.encode_utf8(&mut buf));
        *cursor += c.len_utf8();
        self.normalize();
    }

    /// Insert a paste. Large pastes become atomic chips; small ones are flattened to
    /// a single line (newlines → spaces) and inserted as text — the input box is a
    /// one-line editor, so literal newlines would render as garbage.
    ///
    /// Grok's "paste it again to expand" rule: pasting content identical to an
    /// adjacent chip expands that chip back to text instead of stacking a second chip.
    pub fn insert_paste(&mut self, cursor: &mut usize, raw: &str) {
        let content = raw.replace("\r\n", "\n");
        if content.is_empty() {
            return;
        }
        let lines = content.lines().count().max(1);
        if lines < CHIP_MIN_LINES && content.len() <= CHIP_MIN_BYTES {
            let flat = content.replace('\n', " ");
            self.insert_text_at(*cursor, &flat);
            *cursor += flat.len();
            self.normalize();
            return;
        }
        // Re-paste detection: same content as the chip immediately left or right of
        // the caret → expand that chip in place.
        let layout = self.layout();
        for &(cs, ce, i) in &layout.chips {
            if ce == *cursor || cs == *cursor {
                if let Segment::Paste { content: c, .. } = &self.segments[i] {
                    if *c == content {
                        self.segments[i] = Segment::Text(content.clone());
                        self.normalize();
                        *cursor = cs + content.len();
                        return;
                    }
                }
            }
        }
        let label_len = chip_label(lines).len();
        self.insert_segment_at(*cursor, Segment::Paste { lines, content });
        self.normalize();
        *cursor += label_len;
    }

    pub fn backspace(&mut self, cursor: &mut usize) {
        if *cursor == 0 {
            return;
        }
        // If the caret hugs a chip's right edge, the chip is the deletion unit.
        let layout = self.layout();
        let start =
            if let Some(&(cs, _, _)) = layout.chips.iter().find(|&&(_, ce, _)| ce == *cursor) {
                cs
            } else {
                prev_char_boundary(&layout.display, *cursor)
            };
        self.delete_range(start, *cursor);
        *cursor = start;
    }

    pub fn delete_forward(&mut self, cursor: &mut usize) {
        let layout = self.layout();
        if *cursor >= layout.display.len() {
            return;
        }
        // Caret on a chip's left edge → delete the whole chip.
        let end = if let Some(&(_, ce, _)) = layout.chips.iter().find(|&&(cs, _, _)| cs == *cursor)
        {
            ce
        } else {
            next_char_boundary(&layout.display, *cursor)
        };
        self.delete_range(*cursor, end);
    }

    pub fn move_left(&self, cursor: &mut usize) {
        let layout = self.layout();
        // Landing on a chip from the right skips over it — no caret inside a chip.
        if let Some(&(cs, _, _)) = layout.chips.iter().find(|&&(_, ce, _)| ce == *cursor) {
            *cursor = cs;
            return;
        }
        *cursor = prev_char_boundary(&layout.display, *cursor);
    }

    pub fn move_right(&self, cursor: &mut usize) {
        let layout = self.layout();
        if let Some(&(_, ce, _)) = layout.chips.iter().find(|&&(cs, _, _)| cs == *cursor) {
            *cursor = ce;
            return;
        }
        *cursor = next_char_boundary(&layout.display, *cursor);
    }

    /// Ctrl+W: delete the whitespace-delimited word before the caret. A chip counts as
    /// one word (trailing whitespace between the caret and the chip goes with it).
    pub fn delete_word_back(&mut self, cursor: &mut usize) {
        if *cursor == 0 {
            return;
        }
        let layout = self.layout();
        let left = &layout.display[..*cursor];
        let trimmed_len = left.trim_end_matches(char::is_whitespace).len();
        if trimmed_len == 0 {
            self.delete_range(0, *cursor);
            *cursor = 0;
            return;
        }
        // Word ends exactly at a chip's right edge → the chip is the word.
        if let Some(&(cs, _, _)) = layout.chips.iter().find(|&&(_, ce, _)| ce == trimmed_len) {
            self.delete_range(cs, *cursor);
            *cursor = cs;
            return;
        }
        // Never let the word scan cross into a chip's placeholder: floor the scan at
        // the nearest chip edge before the caret.
        let floor = layout
            .chips
            .iter()
            .filter(|&&(_, ce, _)| ce <= trimmed_len)
            .map(|&(_, ce, _)| ce)
            .max()
            .unwrap_or(0);
        let trimmed = &left[..trimmed_len];
        let start = trimmed
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0)
            .max(floor);
        self.delete_range(start, *cursor);
        *cursor = start;
    }

    /// Ctrl+U: delete everything before the caret.
    pub fn kill_to_start(&mut self, cursor: &mut usize) {
        self.delete_range(0, *cursor);
        *cursor = 0;
    }

    /// Replace the display range `[start, end)` with plain text (completion accept).
    /// Chips intersecting the range go with it; the caret lands after the inserted text.
    pub fn replace_range(&mut self, start: usize, end: usize, text: &str, cursor: &mut usize) {
        self.delete_range(start, end);
        self.insert_text_at(start, text);
        self.normalize();
        *cursor = start + text.len();
    }
}

fn prev_char_boundary(s: &str, offset: usize) -> usize {
    s[..offset]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char_boundary(s: &str, offset: usize) -> usize {
    s[offset..]
        .char_indices()
        .nth(1)
        .map(|(i, _)| offset + i)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paste(lines: usize) -> String {
        (1..=lines)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn buf_with_chip() -> (InputBuffer, usize) {
        // "ab [Pasted: 5 lines] cd", caret at end.
        let mut b = InputBuffer::from_text("ab ");
        let mut cursor = b.display_len();
        b.insert_paste(&mut cursor, &paste(5));
        b.insert_text_at(cursor, " cd");
        cursor += 3;
        (b, cursor)
    }

    #[test]
    fn small_paste_flattens_newlines_to_spaces() {
        let mut b = InputBuffer::new();
        let mut c = 0;
        b.insert_paste(&mut c, "one\ntwo\nthree");
        assert_eq!(b.display(), "one two three");
        assert_eq!(b.expanded(), "one two three");
        assert_eq!(c, b.display_len());
    }

    #[test]
    fn big_paste_becomes_atomic_chip() {
        let (b, _) = buf_with_chip();
        assert_eq!(b.display(), "ab [Pasted: 5 lines] cd");
        // Submit expansion restores the real content.
        assert_eq!(b.expanded(), format!("ab {} cd", paste(5)));
    }

    #[test]
    fn backspace_after_chip_deletes_it_whole() {
        let (mut b, _) = buf_with_chip();
        // Caret right after the chip (before " cd").
        let mut c = "ab [Pasted: 5 lines]".len();
        b.backspace(&mut c);
        assert_eq!(b.display(), "ab  cd");
        assert_eq!(c, 3);
    }

    #[test]
    fn delete_forward_before_chip_deletes_it_whole() {
        let (mut b, _) = buf_with_chip();
        let mut c = 3; // right before the chip
        b.delete_forward(&mut c);
        assert_eq!(b.display(), "ab  cd");
        assert_eq!(c, 3);
    }

    #[test]
    fn caret_skips_over_chips() {
        let (b, _) = buf_with_chip();
        let chip_start = 3;
        let chip_end = 3 + "[Pasted: 5 lines]".len();
        // Moving left from just past the chip lands before it (not inside).
        let mut c = chip_end;
        b.move_left(&mut c);
        assert_eq!(c, chip_start);
        // And back over it to the right.
        b.move_right(&mut c);
        assert_eq!(c, chip_end);
    }

    #[test]
    fn repasting_identical_content_expands_the_chip() {
        let (mut b, _) = buf_with_chip();
        let chip_end = 3 + "[Pasted: 5 lines]".len();
        let mut c = chip_end;
        b.insert_paste(&mut c, &paste(5));
        assert_eq!(b.display(), format!("ab {} cd", paste(5)));
        assert!(b.chip_ranges().is_empty(), "no chip left after expansion");
        assert_eq!(c, 3 + paste(5).len());
    }

    #[test]
    fn repasting_different_content_stacks_a_second_chip() {
        let (mut b, _) = buf_with_chip();
        let chip_end = 3 + "[Pasted: 5 lines]".len();
        let mut c = chip_end;
        b.insert_paste(&mut c, &paste(7));
        assert_eq!(b.display(), "ab [Pasted: 5 lines][Pasted: 7 lines] cd");
    }

    #[test]
    fn ctrl_u_and_ctrl_w_respect_chips() {
        let (mut b, _) = buf_with_chip();
        let chip_end = 3 + "[Pasted: 5 lines]".len();
        // Ctrl+W with caret right after the chip eats the chip as one word.
        let mut c = chip_end;
        b.delete_word_back(&mut c);
        assert_eq!(b.display(), "ab  cd");
        // Ctrl+U from the end clears everything.
        let mut c = b.display_len();
        b.kill_to_start(&mut c);
        assert!(b.is_empty());
        assert_eq!(c, 0);
    }

    #[test]
    fn ctrl_w_never_enters_a_chip_placeholder() {
        // Caret in text AFTER a chip: the word scan must stop at the chip edge.
        let (mut b, _) = buf_with_chip();
        let mut c = b.display_len(); // after "cd"
        b.delete_word_back(&mut c);
        assert_eq!(b.display(), "ab [Pasted: 5 lines] ");
        // Again: caret is at a chip edge → the chip is the word.
        b.delete_word_back(&mut c);
        assert_eq!(b.display(), "ab ");
    }

    #[test]
    fn typing_around_chips_merges_into_neighboring_text() {
        let (mut b, _) = buf_with_chip();
        let chip_end = 3 + "[Pasted: 5 lines]".len();
        let mut c = chip_end;
        b.insert_char(&mut c, 'X');
        assert_eq!(b.display(), "ab [Pasted: 5 lines]X cd");
        let mut c = 3; // before the chip
        b.insert_char(&mut c, 'Y');
        assert_eq!(b.display(), "ab Y[Pasted: 5 lines]X cd");
    }

    #[test]
    fn replace_range_spans_chips_for_completion_accept() {
        let (mut b, _) = buf_with_chip();
        let mut c = b.display_len();
        b.replace_range(4, 9, "ZZ", &mut c); // cuts into the chip's placeholder
        assert_eq!(b.display(), "ab  ZZcd");
        assert_eq!(c, 6);
    }

    #[test]
    fn multibyte_text_stays_on_char_boundaries() {
        let mut b = InputBuffer::from_text("汉字x");
        let mut c = b.display_len();
        b.backspace(&mut c);
        assert_eq!(b.display(), "汉字");
        b.move_left(&mut c);
        assert_eq!(c, "汉".len());
        let mut end = b.display_len();
        b.insert_paste(&mut end, &paste(4));
        assert_eq!(b.display(), "汉字[Pasted: 4 lines]");
        b.move_left(&mut end);
        assert_eq!(end, "汉字".len(), "skip the chip as one unit");
    }

    #[test]
    fn byte_sized_paste_chips_even_with_few_lines() {
        let mut b = InputBuffer::new();
        let mut c = 0;
        let big = "x".repeat(CHIP_MIN_BYTES + 1);
        b.insert_paste(&mut c, &big);
        assert_eq!(b.display(), "[Pasted: 1 lines]");
        assert_eq!(b.expanded(), big);
    }
}
