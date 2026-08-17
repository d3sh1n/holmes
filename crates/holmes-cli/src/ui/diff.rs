//! Unified diff rendering for `edit_file` / `write_file` tool blocks.
//!
//! Holmes's tools return only a JSON summary (`{"path", "replacements"}`), so the diff
//! shown in scrollback is rebuilt from the tool *arguments*: `TextDiff::from_lines`
//! over the old/new strings (grok's fallback path works the same way). Rendering
//! follows grok's model, flattened into plain scrollback `Line`s:
//!
//! - no `+`/`-` prefixes — insert/delete rows carry a full-row background band from the
//!   theme (`diff_insert_bg` / `diff_delete_bg`), context rows are unbanded;
//! - a right-aligned line-number gutter; hunks are separated by `… N unchanged lines`;
//! - rows are truncated with `…` at the terminal width instead of wrapped, because
//!   `fit_lines` wrapping a banded row would split the band and misalign the gutter.
//!
//! Line numbers: the tools don't report where an edit landed, so after a successful
//! `edit_file` the file is re-read and `new_string` located in it — trusted only when
//! the match is unique (`replace_all` or repeated text makes coordinates lie, so the
//! gutter stays blank but keeps its width). `write_file` numbers are trivially 1-based.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};
use unicode_width::UnicodeWidthChar;

use crate::ui::blocks::DisplayMode;
use crate::ui::highlight;
use crate::ui::theme::theme;

/// Context rows kept around each change cluster (grok uses the same ±3).
const CONTEXT: usize = 3;
/// `write_file` in Truncated mode shows this many leading lines, then `… +N lines`.
const WRITE_PREVIEW_LINES: usize = 10;
/// Left margin aligning the diff under the block title's text.
const INDENT: &str = "  ";

/// Which side of the diff a row belongs to (drives band color and the fallback
/// foreground when highlighting is unavailable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Context,
    Delete,
    Insert,
}

#[derive(Clone, Debug)]
struct Row {
    side: Side,
    text: String,
    /// 1-based line number within the *new* text; 0 for deletes (no new-side
    /// coordinate exists for a removed line).
    rel_new: usize,
}

/// A built diff: hunks of rows, plus gaps recording how many unchanged rows were
/// elided between hunks.
enum Item {
    Hunk(Vec<Row>),
    Gap(usize),
}

/// One output row after display-mode selection: a content row, an intra-diff gap
/// marker, or a trailing overflow marker (`… +N more hunks` / `… +N lines`).
enum Out {
    Row(Row),
    Gap(usize),
    Marker(String),
}

/// Render the body of a successful `edit_file` block from its raw args JSON.
/// `None` on any parse/diff failure — the caller then falls back to the plain output
/// body, so a broken diff can never break the block.
pub fn render_edit_file(
    args_json: &str,
    mode: DisplayMode,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    let args: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let path = args.get("path")?.as_str()?;
    let old = args.get("old_string")?.as_str()?;
    let new = args.get("new_string")?.as_str()?;
    let items = build_items(old, new);
    if items.is_empty() {
        // Identical strings produce no changes (the tool rejects those, but a UI
        // rendering path must degrade, not panic).
        return None;
    }
    let base = locate_start_line(path, new);

    let mut outs: Vec<Out> = Vec::new();
    match mode {
        // Truncated: first hunk only, then how many more remain.
        DisplayMode::Truncated => {
            let mut iter = items.into_iter();
            if let Some(Item::Hunk(rows)) = iter.next() {
                outs.extend(rows.into_iter().map(Out::Row));
            }
            let more = iter.filter(|i| matches!(i, Item::Hunk(_))).count();
            if more > 0 {
                let plural = if more == 1 { "hunk" } else { "hunks" };
                outs.push(Out::Marker(format!("… +{more} more {plural}")));
            }
        }
        // Collapsed never reaches the body renderer; treat it like Expanded here.
        _ => {
            for item in items {
                match item {
                    Item::Hunk(rows) => outs.extend(rows.into_iter().map(Out::Row)),
                    Item::Gap(n) => outs.push(Out::Gap(n)),
                }
            }
        }
    }
    Some(emit(path, &outs, base, width))
}

/// Render the body of a successful `write_file` block: the tool result carries no
/// previous content, so an overwrite renders as a fresh all-insert write (same shape
/// as a new file) rather than a fabricated diff.
pub fn render_write_file(
    args_json: &str,
    mode: DisplayMode,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    let args: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let path = args.get("path")?.as_str()?;
    let content = args.get("content")?.as_str()?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Some(vec![meta_line("(empty file)".to_string())]);
    }
    let mut outs: Vec<Out> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            Out::Row(Row {
                side: Side::Insert,
                text: (*l).to_string(),
                rel_new: i + 1,
            })
        })
        .collect();
    if mode == DisplayMode::Truncated && outs.len() > WRITE_PREVIEW_LINES {
        let hidden = outs.len() - WRITE_PREVIEW_LINES;
        outs.truncate(WRITE_PREVIEW_LINES);
        outs.push(Out::Marker(format!("… +{hidden} lines")));
    }
    Some(emit(path, &outs, Some(1), width))
}

/// Diff `old` against `new` and cut the result into context-bounded hunks.
///
/// Changes separated by at most `2 * CONTEXT` equal rows merge into one hunk (their
/// context windows would touch anyway); wider separations split, and the elided equal
/// rows are recorded as a `Gap`. Leading/trailing context beyond `CONTEXT` at the
/// fragment edges is trimmed silently — there is no "unchanged" run to report at a
/// fragment boundary.
fn build_items(old: &str, new: &str) -> Vec<Item> {
    let diff = TextDiff::from_lines(old, new);
    let mut rows: Vec<Row> = Vec::new();
    let mut rel_new = 1usize;
    for change in diff.iter_all_changes() {
        let text = change.value().trim_end_matches(['\r', '\n']).to_string();
        match change.tag() {
            ChangeTag::Equal => {
                rows.push(Row {
                    side: Side::Context,
                    text,
                    rel_new,
                });
                rel_new += 1;
            }
            ChangeTag::Insert => {
                rows.push(Row {
                    side: Side::Insert,
                    text,
                    rel_new,
                });
                rel_new += 1;
            }
            ChangeTag::Delete => rows.push(Row {
                side: Side::Delete,
                text,
                rel_new: 0,
            }),
        }
    }

    let changed: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.side != Side::Context)
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }
    // Merge change indices into clusters spanning the equal runs between them.
    let mut clusters: Vec<(usize, usize)> = vec![(changed[0], changed[0])];
    for &i in &changed[1..] {
        let last = clusters.last_mut().expect("one cluster seeded");
        if i - last.1 - 1 <= 2 * CONTEXT {
            last.1 = i;
        } else {
            clusters.push((i, i));
        }
    }

    let mut items = Vec::new();
    let mut prev_end = 0usize; // first row index not yet covered by a hunk
    for (idx, (first, last)) in clusters.iter().enumerate() {
        let start = first.saturating_sub(CONTEXT);
        let end = (last + CONTEXT + 1).min(rows.len());
        if idx > 0 && start > prev_end {
            items.push(Item::Gap(start - prev_end));
        }
        items.push(Item::Hunk(rows[start..end].to_vec()));
        prev_end = end;
    }
    items
}

/// Find the 1-based line where `new_string` starts in the on-disk file — the edit has
/// already been applied by the time we render. Only a *unique* match is trustworthy:
/// repeated text (or `replace_all`) means several edit sites, and picking one would
/// show wrong numbers, so the caller leaves the gutter blank instead.
fn locate_start_line(path: &str, new_string: &str) -> Option<usize> {
    let content = std::fs::read_to_string(path).ok()?;
    locate_in(&content, new_string)
}

/// Pure core of [`locate_start_line`] (tests inject content directly).
fn locate_in(content: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let mut matches = content.match_indices(needle);
    let (pos, _) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(content[..pos].bytes().filter(|&b| b == b'\n').count() + 1)
}

/// The real new-file line number for a row, if the edit site was located.
fn real_line(base: Option<usize>, rel_new: usize) -> Option<usize> {
    if rel_new == 0 {
        return None;
    }
    base.map(|b| b + rel_new - 1)
}

/// Flatten selected output rows into styled scrollback lines.
fn emit(path: &str, outs: &[Out], base: Option<usize>, width: usize) -> Vec<Line<'static>> {
    // Gutter width comes from the largest line number actually shown (real when the
    // edit site was located, fragment-relative otherwise) so a blank-but-aligned
    // gutter still lines up.
    let max_line = outs
        .iter()
        .filter_map(|o| match o {
            Out::Row(r) => real_line(base, r.rel_new).or(Some(r.rel_new).filter(|&n| n > 0)),
            _ => None,
        })
        .max()
        .unwrap_or(1);
    let gutter_w = digits(max_line);
    let body_budget = width.saturating_sub(INDENT.len() + gutter_w + 1).max(1);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut i = 0;
    while i < outs.len() {
        match &outs[i] {
            Out::Gap(n) => {
                lines.push(meta_line(format!("… {n} unchanged lines")));
                i += 1;
            }
            Out::Marker(m) => {
                lines.push(meta_line(m.clone()));
                i += 1;
            }
            Out::Row(_) => {
                // A contiguous run of rows is one hunk. Highlight the delete side and
                // the context+insert side with separate `HighlightLines` instances so a
                // multi-line construct (string, block comment) can't leak across the
                // -/+ seam (grok's hunk-only model).
                let start = i;
                while i < outs.len() && matches!(outs[i], Out::Row(_)) {
                    i += 1;
                }
                let run = &outs[start..i];
                let new_side: Vec<&str> = run
                    .iter()
                    .filter_map(|o| match o {
                        Out::Row(r) if r.side != Side::Delete => Some(r.text.as_str()),
                        _ => None,
                    })
                    .collect();
                let old_side: Vec<&str> = run
                    .iter()
                    .filter_map(|o| match o {
                        Out::Row(r) if r.side == Side::Delete => Some(r.text.as_str()),
                        _ => None,
                    })
                    .collect();
                let new_hi = highlight::highlight_lines(path, &new_side);
                let old_hi = highlight::highlight_lines(path, &old_side);
                let (mut ni, mut oi) = (0usize, 0usize);
                for o in run {
                    let Out::Row(row) = o else { continue };
                    let tokens = match row.side {
                        Side::Delete => {
                            let t = old_hi.as_ref().map(|h| h[oi].clone());
                            oi += 1;
                            t
                        }
                        _ => {
                            let t = new_hi.as_ref().map(|h| h[ni].clone());
                            ni += 1;
                            t
                        }
                    };
                    lines.push(row_line(row, tokens, gutter_w, body_budget, base));
                }
            }
        }
    }
    lines
}

/// One content row: `  <gutter> <body>` where the body is highlighted tokens over the
/// per-side background band (or a flat fallback color when highlighting is off).
fn row_line(
    row: &Row,
    tokens: Option<Vec<(Style, String)>>,
    gutter_w: usize,
    budget: usize,
    base: Option<usize>,
) -> Line<'static> {
    let t = theme();
    let (base_fg, band) = match row.side {
        Side::Context => (t.diff_context_fg, None),
        Side::Delete => (t.diff_delete_fg, Some(t.diff_delete_bg)),
        Side::Insert => (t.diff_insert_fg, Some(t.diff_insert_bg)),
    };
    let gutter = match real_line(base, row.rel_new) {
        Some(n) => format!("{n:>gutter_w$}"),
        // Deletes have no new-file coordinate; an unlocatable edit leaves the whole
        // gutter blank. Both keep the width so columns stay aligned.
        None => " ".repeat(gutter_w),
    };
    let mut spans = vec![
        Span::raw(INDENT.to_string()),
        Span::styled(format!("{gutter} "), Style::default().fg(t.diff_gutter_fg)),
    ];
    let tokens = tokens.unwrap_or_else(|| vec![(Style::default(), row.text.clone())]);
    let mut body: Vec<(Style, String)> = tokens
        .into_iter()
        .map(|(st, txt)| {
            let mut s = Style::default()
                .fg(st.fg.unwrap_or(base_fg))
                .add_modifier(st.add_modifier);
            if let Some(bg) = band {
                // On 16-color themes the band quantizes to Reset — harmless; the flat
                // red/green foreground is the signal there.
                s = s.bg(bg);
            }
            (s, txt)
        })
        .collect();
    truncate_tokens(&mut body, budget);
    spans.extend(body.into_iter().map(|(st, txt)| Span::styled(txt, st)));
    Line::from(spans)
}

/// A separator/overflow line (`… N unchanged lines`, `… +N more hunks`, …).
fn meta_line(text: String) -> Line<'static> {
    Line::from(Span::styled(
        format!("{INDENT}{text}"),
        Style::default().fg(theme().diff_gutter_fg),
    ))
}

/// Hard-truncate styled tokens to `budget` display columns, closing with `…` in the
/// cut token's style (keeps the background band intact up to the ellipsis).
fn truncate_tokens(tokens: &mut Vec<(Style, String)>, budget: usize) {
    let mut used = 0usize;
    for i in 0..tokens.len() {
        let w: usize = tokens[i].1.chars().map(|c| c.width().unwrap_or(0)).sum();
        if used + w > budget {
            let keep = budget.saturating_sub(used).saturating_sub(1);
            let (mut cut, mut cw) = (String::new(), 0usize);
            for ch in tokens[i].1.chars() {
                let chw = ch.width().unwrap_or(0);
                if cw + chw > keep {
                    break;
                }
                cut.push(ch);
                cw += chw;
            }
            cut.push('…');
            let st = tokens[i].0;
            tokens.truncate(i);
            tokens.push((st, cut));
            return;
        }
        used += w;
    }
}

fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
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

    fn width_of(line: &Line) -> usize {
        line.spans
            .iter()
            .flat_map(|s| s.content.chars())
            .map(|c| c.width().unwrap_or(0))
            .sum()
    }

    fn numbered(start: usize, end: usize) -> String {
        (start..=end).map(|i| format!("l{i}\n")).collect()
    }

    fn args(path: &str, old: &str, new: &str) -> String {
        serde_json::json!({"path": path, "old_string": old, "new_string": new}).to_string()
    }

    // ── hunk cutting / merging ──

    #[test]
    fn single_change_yields_one_hunk_with_context() {
        let items = build_items("a\nb\nc\nd\ne", "a\nB\nc\nd\ne");
        assert_eq!(items.len(), 1);
        let Item::Hunk(rows) = &items[0] else {
            panic!("expected a hunk")
        };
        let sides: Vec<Side> = rows.iter().map(|r| r.side).collect();
        assert_eq!(
            sides,
            vec![
                Side::Context,
                Side::Delete,
                Side::Insert,
                Side::Context,
                Side::Context,
                Side::Context,
            ]
        );
        assert_eq!(rows[2].text, "B");
        assert_eq!(rows[2].rel_new, 2);
    }

    #[test]
    fn distant_changes_split_with_gap_marker() {
        // Changes at lines 2 and 19 of 20: the equal run between them (16 rows) is way
        // past 2*CONTEXT, so two hunks with the elided count in between.
        let old = numbered(1, 20);
        let new = old
            .replacen("l2\n", "X2\n", 1)
            .replacen("l19\n", "X19\n", 1);
        let items = build_items(&old, &new);
        assert_eq!(items.len(), 3);
        let Item::Hunk(first) = &items[0] else {
            panic!()
        };
        let Item::Gap(n) = &items[1] else {
            panic!("expected a gap")
        };
        let Item::Hunk(second) = &items[2] else {
            panic!()
        };
        // First hunk: the replacement (delete+insert at idx 1-2) + 3 context after →
        // rows 0..=5; the second starts at idx 15+1 (insert shifts indices).
        assert_eq!(first.len(), 6);
        assert_eq!(*n, 10, "16 unchanged rows minus 2*3 shown as context");
        assert!(second.iter().any(|r| r.text == "X19"));
    }

    #[test]
    fn adjacent_changes_merge_into_one_hunk() {
        // Changes at idx 1 and idx 5: 3 equal rows between them ≤ 2*CONTEXT → merged.
        let old = numbered(1, 10);
        let new = old.replacen("l2\n", "X2\n", 1).replacen("l6\n", "X6\n", 1);
        let items = build_items(&old, &new);
        assert_eq!(items.len(), 1, "context windows touching must merge");
        let Item::Hunk(rows) = &items[0] else {
            panic!()
        };
        assert!(rows.iter().any(|r| r.text == "X2"));
        assert!(rows.iter().any(|r| r.text == "X6"));
    }

    #[test]
    fn identical_strings_produce_no_items() {
        assert!(build_items("same\ntext\n", "same\ntext\n").is_empty());
    }

    // ── line-number location ──

    #[test]
    fn locate_unique_match_gives_1_based_line() {
        assert_eq!(locate_in("one\ntwo\nthree\n", "three"), Some(3));
        assert_eq!(locate_in("needle at start", "needle"), Some(1));
    }

    #[test]
    fn locate_repeated_or_empty_match_is_untrusted() {
        assert_eq!(locate_in("dup\ndup\n", "dup"), None);
        assert_eq!(locate_in("anything", ""), None);
        assert_eq!(locate_in("no match here", "absent"), None);
    }

    #[test]
    fn real_line_numbers_offset_from_located_base() {
        let out = render_edit_file(
            &args("x.unknownext", "l2\n", "X2\n"),
            DisplayMode::Expanded,
            80,
        );
        // No file on disk at "x.unknownext" → gutter blank. Now simulate a located
        // base via emit directly: rel 2 with base 40 → real 41.
        assert!(out.is_some());
        let rows = vec![Out::Row(Row {
            side: Side::Insert,
            text: "x".into(),
            rel_new: 2,
        })];
        let lines = emit("x", &rows, Some(40), 80);
        assert!(
            plain(&lines)[0].starts_with("  41 "),
            "got {:?}",
            plain(&lines)[0]
        );
    }

    #[test]
    fn gutter_width_follows_largest_shown_number() {
        // base 95, a two-line insert → max shown 96… force width 3 with base 995.
        let rows = vec![
            Out::Row(Row {
                side: Side::Context,
                text: "ctx".into(),
                rel_new: 1,
            }),
            Out::Row(Row {
                side: Side::Delete,
                text: "old".into(),
                rel_new: 0,
            }),
            Out::Row(Row {
                side: Side::Insert,
                text: "new".into(),
                rel_new: 2,
            }),
        ];
        let lines = emit("x", &rows, Some(995), 80);
        let text = plain(&lines);
        // 3-digit gutter, right-aligned, one trailing space after it.
        assert!(text[0].starts_with("  995 "), "got {:?}", text[0]);
        assert!(text[2].starts_with("  996 "), "got {:?}", text[2]);
        // Delete row: blank gutter, same width → indent(2) + 3 spaces + 1 space.
        assert!(text[1].starts_with("      old"), "got {:?}", text[1]);
    }

    #[test]
    fn unknown_location_keeps_gutter_blank_but_aligned() {
        let rows = vec![
            Out::Row(Row {
                side: Side::Context,
                text: "ctx".into(),
                rel_new: 1,
            }),
            Out::Row(Row {
                side: Side::Insert,
                text: "new".into(),
                rel_new: 2,
            }),
        ];
        let lines = emit("x", &rows, None, 80);
        let text = plain(&lines);
        // Width still derived from the fragment-relative numbers (max 2 → 1 col).
        assert!(text[0].starts_with("    ctx"), "got {:?}", text[0]);
        assert!(text[1].starts_with("    new"), "got {:?}", text[1]);
    }

    // ── display modes ──

    #[test]
    fn truncated_shows_first_hunk_and_counts_the_rest() {
        let old = numbered(1, 20);
        let new = old
            .replacen("l2\n", "X2\n", 1)
            .replacen("l19\n", "X19\n", 1);
        let json = args("f.unknownext", &old, &new);
        let text = plain(&render_edit_file(&json, DisplayMode::Truncated, 80).unwrap());
        assert!(text.iter().any(|l| l.contains("X2")));
        assert!(
            !text.iter().any(|l| l.contains("X19")),
            "second hunk hidden"
        );
        assert_eq!(text.last().unwrap().trim(), "… +1 more hunk");
    }

    #[test]
    fn expanded_shows_all_hunks_and_the_gap() {
        let old = numbered(1, 20);
        let new = old
            .replacen("l2\n", "X2\n", 1)
            .replacen("l19\n", "X19\n", 1);
        let json = args("f.unknownext", &old, &new);
        let text = plain(&render_edit_file(&json, DisplayMode::Expanded, 80).unwrap());
        assert!(text.iter().any(|l| l.contains("X2")));
        assert!(text.iter().any(|l| l.contains("X19")));
        assert!(text.iter().any(|l| l.trim() == "… 10 unchanged lines"));
    }

    #[test]
    fn write_file_truncated_previews_ten_lines() {
        let content = numbered(1, 15);
        let json = serde_json::json!({"path": "o.unknownext", "content": content}).to_string();
        let text = plain(&render_write_file(&json, DisplayMode::Truncated, 80).unwrap());
        assert_eq!(text.len(), 10 + 1);
        assert!(text[0].starts_with("   1 l1"), "got {:?}", text[0]);
        assert_eq!(text.last().unwrap().trim(), "… +5 lines");
    }

    #[test]
    fn write_file_expanded_numbers_from_one() {
        let content = numbered(1, 12);
        let json = serde_json::json!({"path": "o.unknownext", "content": content}).to_string();
        let text = plain(&render_write_file(&json, DisplayMode::Expanded, 80).unwrap());
        assert_eq!(text.len(), 12);
        // Width 2 gutter: line 12 must align under line 1's text.
        assert!(text[0].starts_with("   1 l1"), "got {:?}", text[0]);
        assert!(text[11].starts_with("  12 l12"), "got {:?}", text[11]);
    }

    #[test]
    fn write_file_empty_content_renders_marker() {
        let json = serde_json::json!({"path": "o.txt", "content": ""}).to_string();
        let text = plain(&render_write_file(&json, DisplayMode::Expanded, 80).unwrap());
        assert_eq!(text, vec!["  (empty file)"]);
    }

    // ── styling ──

    #[test]
    fn insert_and_delete_rows_carry_background_bands() {
        // Unknown extension → no syntect → flat fallback foregrounds over the bands.
        let json = args("f.unknownext", "old line\n", "new line\n");
        let lines = render_edit_file(&json, DisplayMode::Expanded, 80).unwrap();
        let t = theme();
        let find_row = |needle: &str| {
            lines
                .iter()
                .find(|l| l.spans.iter().any(|s| s.content.contains(needle)))
                .unwrap()
                .clone()
        };
        let del = find_row("old line");
        let ins = find_row("new line");
        let body = |l: &Line| l.spans.last().unwrap().style;
        assert_eq!(body(&del).bg, Some(t.diff_delete_bg));
        assert_eq!(body(&del).fg, Some(t.diff_delete_fg));
        assert_eq!(body(&ins).bg, Some(t.diff_insert_bg));
        assert_eq!(body(&ins).fg, Some(t.diff_insert_fg));
    }

    #[test]
    fn known_extension_gets_syntect_foreground_over_band() {
        // Tests never run init_theme → TrueColor fallback → highlighting active.
        let json = args("f.rs", "fn old() {}\n", "fn new() {}\n");
        let lines = render_edit_file(&json, DisplayMode::Expanded, 80).unwrap();
        let ins = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("new")))
            .unwrap();
        let body_fg = ins.spans[2].style.fg;
        assert!(matches!(body_fg, Some(Color::Rgb(..))), "got {body_fg:?}");
        assert_eq!(ins.spans[2].style.bg, Some(theme().diff_insert_bg));
    }

    #[test]
    fn overlong_rows_truncate_with_ellipsis_instead_of_wrapping() {
        let long = format!("{}\n", "x".repeat(100));
        let json = args("f.unknownext", "short\n", &long);
        let lines = render_edit_file(&json, DisplayMode::Expanded, 30).unwrap();
        for l in &lines {
            assert!(
                width_of(l) <= 30,
                "row exceeds width: {:?}",
                plain(std::slice::from_ref(l))
            );
        }
        let ins = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains('x')))
            .unwrap();
        let text: String = ins.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with('…'), "got {text:?}");
    }

    // ── robustness ──

    #[test]
    fn malformed_args_yield_none() {
        assert!(render_edit_file("not json", DisplayMode::Expanded, 80).is_none());
        assert!(render_edit_file(r#"{"path":"x"}"#, DisplayMode::Expanded, 80).is_none());
        assert!(render_write_file(r#"{"path":"x"}"#, DisplayMode::Expanded, 80).is_none());
        // Identical strings: nothing to show.
        assert!(render_edit_file(&args("x", "a\n", "a\n"), DisplayMode::Expanded, 80).is_none());
    }

    #[test]
    fn located_edit_uses_real_file_line_numbers() {
        // Simulate a completed edit: the file on disk contains new_string exactly once.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("holmes-diff-test-{}-loc.txt", std::process::id()));
        std::fs::write(&path, "header\nfn new() {}\nfooter\n").unwrap();
        let json = args(path.to_str().unwrap(), "fn old() {}\n", "fn new() {}\n");
        let text = plain(&render_edit_file(&json, DisplayMode::Expanded, 80).unwrap());
        std::fs::remove_file(&path).ok();
        let ins = text.iter().find(|l| l.contains("fn new()")).unwrap();
        assert!(ins.starts_with("  2 "), "real line number, got {ins:?}");
    }
}
