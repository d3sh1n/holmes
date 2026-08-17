//! Word-aware, style-preserving line wrapping.
//!
//! Scrollback is committed via `Terminal::insert_before`, which needs the exact row
//! count up front — so lines are wrapped to the terminal width ourselves instead of
//! relying on ratatui's `Paragraph`. Unlike a hard character wrap, this breaks ASCII
//! text at word boundaries (spaces) and only hard-breaks a word that is wider than
//! the whole line; CJK text may break between any two characters (measured by display
//! width, so wide chars count as two columns). Span styles survive the wrap, and the
//! space a line was broken at is dropped (no trailing whitespace in scrollback).

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// Wrap styled lines to `width` display columns, preserving per-span styling.
pub fn fit_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in lines {
        // Flatten to (char, style) so wrap decisions ignore span boundaries; spans are
        // re-grouped by style when a row is emitted.
        let mut chars: Vec<(char, Style)> = Vec::new();
        for span in line.spans {
            for ch in span.content.chars() {
                chars.push((ch, span.style));
            }
        }
        wrap_chars(&chars, width, &mut out);
    }
    out
}

/// Greedily wrap one flattened line into `out`, always emitting at least one row
/// (an empty input line stays an empty row).
fn wrap_chars(chars: &[(char, Style)], width: usize, out: &mut Vec<Line<'static>>) {
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0usize;
    // Rightmost position `cur` may be split at (chars before it form the emitted row).
    // A break *at a space* consumes that space; a break around a wide (CJK) char keeps it.
    let mut break_at: Option<usize> = None;
    // Just wrapped: swallow spaces at the start of the fresh row so a break never
    // produces rows that are blank or space-indented.
    let mut fresh_row = false;

    let mut i = 0;
    while i < chars.len() {
        let (ch, style) = chars[i];
        if fresh_row && ch == ' ' {
            i += 1;
            continue;
        }
        fresh_row = false;
        let cw = char_width(ch);
        if cur_w + cw > width && !cur.is_empty() {
            if ch == ' ' || cw == 2 {
                // Break exactly here: the row is full and this char starts the next row
                // (a space is consumed, a CJK char is carried over). Always at least as
                // good as any earlier break point.
                emit_row(&cur, out);
                cur.clear();
                cur_w = 0;
                break_at = None;
                fresh_row = true;
                if ch == ' ' {
                    i += 1;
                }
                continue;
            }
            match break_at {
                Some(b) => {
                    emit_row(&cur[..b], out);
                    let tail: Vec<(char, Style)> = cur[b..].to_vec();
                    cur = tail;
                    cur_w = display_width(&cur);
                    break_at = last_break(&cur);
                    fresh_row = true;
                }
                // No break opportunity on this row: the word is wider than the line —
                // hard-break it rather than overflow the terminal.
                None => {
                    emit_row(&cur, out);
                    cur.clear();
                    cur_w = 0;
                    fresh_row = true;
                }
            }
            continue; // retry the same char on the new row
        }
        cur.push((ch, style));
        cur_w += cw;
        if ch == ' ' || cw == 2 {
            break_at = Some(cur.len());
        }
        i += 1;
    }
    emit_row(&cur, out);
}

fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

fn display_width(chars: &[(char, Style)]) -> usize {
    chars.iter().map(|(ch, _)| char_width(*ch)).sum()
}

/// Rightmost split position in `row` (after a space or a wide char).
fn last_break(row: &[(char, Style)]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, (ch, _)) in row.iter().enumerate() {
        if *ch == ' ' || char_width(*ch) == 2 {
            best = Some(i + 1);
        }
    }
    best
}

/// Emit one wrapped row, re-grouping contiguous same-style chars into spans.
/// Trailing spaces are collapsed (they'd only add ragged whitespace to scrollback).
fn emit_row(row: &[(char, Style)], out: &mut Vec<Line<'static>>) {
    let mut end = row.len();
    while end > 0 && row[end - 1].0 == ' ' {
        end -= 1;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for &(ch, style) in &row[..end] {
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push(ch),
            _ => spans.push(Span::styled(ch.to_string(), style)),
        }
    }
    out.push(Line::from(spans));
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

    fn width_of(s: &str) -> usize {
        s.chars().map(char_width).sum()
    }

    #[test]
    fn overlong_word_still_hard_wraps() {
        // No spaces anywhere → the only option is a hard break at the width.
        let out = fit_lines(vec![Line::from("a".repeat(25))], 10);
        let texts = plain(&out);
        assert_eq!(texts, vec!["a".repeat(10), "a".repeat(10), "a".repeat(5)]);
    }

    #[test]
    fn ascii_wraps_at_word_boundaries() {
        let out = fit_lines(vec![Line::from("hello world foo".to_string())], 11);
        let texts = plain(&out);
        // Greedy: "hello world" exactly fills 11 cols; words are never split.
        assert_eq!(texts, vec!["hello world", "foo"]);
    }

    #[test]
    fn word_wrap_prefers_latest_space() {
        let out = fit_lines(vec![Line::from("aaa bbb ccc".to_string())], 7);
        let texts = plain(&out);
        assert_eq!(texts, vec!["aaa bbb", "ccc"]);
    }

    #[test]
    fn break_drops_the_breaking_space() {
        let out = fit_lines(vec![Line::from("aaaa bb cc".to_string())], 6);
        let texts = plain(&out);
        assert_eq!(texts, vec!["aaaa", "bb cc"]);
        assert!(texts.iter().all(|t| !t.ends_with(' ')));
    }

    #[test]
    fn cjk_breaks_between_any_chars_by_display_width() {
        // 8 CJK chars = 16 display columns; width 10 → two rows of 5 chars.
        let out = fit_lines(vec![Line::from("你好世界你好世界".to_string())], 10);
        let texts = plain(&out);
        assert_eq!(texts, vec!["你好世界你", "好世界"]);
    }

    #[test]
    fn mixed_cjk_and_ascii_words() {
        // CJK chars are their own break opportunities; ASCII words stay whole.
        let out = fit_lines(vec![Line::from("你好 abc 你好".to_string())], 7);
        let texts = plain(&out);
        // Greedy fill: "abc 你" is 6 cols and fits; only the last CJK char wraps.
        assert_eq!(texts, vec!["你好", "abc 你", "好"]);
        assert!(texts.iter().all(|t| width_of(t) <= 7));
        assert!(texts.iter().all(|t| !t.ends_with(' ')));
    }

    #[test]
    fn preserves_span_styles_across_wrap() {
        let styled = Line::from(vec![
            Span::styled("aaaa bbbb ".to_string(), Style::default().fg(Color::Green)),
            Span::styled("cccc".to_string(), Style::default().fg(Color::Red)),
        ]);
        let out = fit_lines(vec![styled], 9);
        // "aaaa bbbb" fills row 1 (9 cols); "cccc" wraps to row 2 keeping Red.
        assert_eq!(out.len(), 2);
        assert_eq!(plain(&out)[0], "aaaa bbbb");
        assert!(out[0]
            .spans
            .iter()
            .all(|s| s.style.fg == Some(Color::Green)));
        assert_eq!(plain(&out)[1], "cccc");
        assert_eq!(out[1].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn empty_line_stays_empty() {
        let out = fit_lines(vec![Line::from(""), Line::from("x".to_string())], 10);
        assert_eq!(out.len(), 2);
        assert_eq!(plain(&out)[0], "");
        assert_eq!(plain(&out)[1], "x");
    }

    #[test]
    fn width_one_boundary() {
        let out = fit_lines(vec![Line::from("ab c".to_string())], 1);
        let texts = plain(&out);
        // Every ASCII char gets its own row at width 1; the space is a break point.
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    #[test]
    fn width_one_wide_char_does_not_loop_forever() {
        // A 2-column char can never fit in width 1; it must still be emitted.
        let out = fit_lines(vec![Line::from("你".to_string())], 1);
        assert_eq!(plain(&out), vec!["你"]);
    }

    #[test]
    fn short_line_is_untouched() {
        let out = fit_lines(vec![Line::from("short".to_string())], 80);
        assert_eq!(plain(&out), vec!["short"]);
    }
}
