//! Structured tool blocks: verb-title lines, three-state folding, and verb-group
//! aggregation.
//!
//! A `ToolBlock` is the UI-side record of one tool call, paired by `call_id` across
//! `ToolStarted` / `ToolFinished`. Running blocks live in the live region (status line
//! with a wave animation); only *finished* blocks are committed to scrollback, because
//! scrollback is immutable once inserted (`insert_before`). To keep the transcript
//! quiet during read-heavy recon, consecutive finished read-only blocks that render as
//! a single collapsed line are buffered and flushed as one aggregate line
//! (`Read 3 files · Searched 1 pattern`) as soon as anything non-aggregatable arrives.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::diff;
use crate::ui::theme::{glyphs, theme};

/// Lifecycle of a tool call as the UI sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Err,
}

/// How much of a finished block's output is committed to scrollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayMode {
    /// Title line only.
    Collapsed,
    /// Title + first 8 output lines + `… +N lines`.
    Truncated,
    /// Title + up to 40 output lines + `… +N more`.
    Expanded,
}

/// One tool call, from `ToolStarted` (Running) to `ToolFinished` (Ok/Err).
#[derive(Clone, Debug)]
pub struct ToolBlock {
    pub call_id: Option<String>,
    pub name: String,
    /// Raw arguments JSON as delivered by `ToolStarted` (may be absent or malformed —
    /// every consumer degrades gracefully to the bare tool name).
    pub args_json: Option<String>,
    pub status: ToolStatus,
    pub output: String,
    pub display_mode: DisplayMode,
}

impl ToolBlock {
    pub fn running(name: String, call_id: Option<String>, args_json: Option<String>) -> Self {
        ToolBlock {
            call_id,
            name,
            args_json,
            status: ToolStatus::Running,
            output: String::new(),
            display_mode: DisplayMode::Collapsed,
        }
    }

    /// The title line as committed to scrollback for a finished block: a ✓/✗ status
    /// mark, then the verb/title spans.
    pub fn title_line(&self) -> Line<'static> {
        let (mark, colour) = match self.status {
            ToolStatus::Ok => ("✓", theme().success),
            ToolStatus::Err => ("✗", theme().failure),
            ToolStatus::Running => ("⚙", theme().text_faint),
        };
        let mut spans = vec![
            Span::raw("  ".to_string()),
            Span::styled(format!("{mark} "), Style::default().fg(colour)),
        ];
        spans.extend(title_spans(&self.name, self.args_json.as_deref()));
        Line::from(spans)
    }

    /// Plain-text title for the live-region status line (no status mark).
    pub fn title_text(&self) -> String {
        let t = verb_title(&self.name, self.args_json.as_deref());
        let mut s = t.verb;
        if !t.target.is_empty() {
            s.push(' ');
            s.push_str(&t.target);
        }
        let suffix = t.suffix.text();
        if !suffix.is_empty() {
            s.push(' ');
            s.push_str(&suffix);
        }
        s
    }

    /// Render the finished block according to its `DisplayMode`.
    pub fn render(&self) -> Vec<Line<'static>> {
        let mut lines = vec![self.title_line()];
        if self.display_mode == DisplayMode::Collapsed {
            return lines;
        }
        // Successful file writes show the diff of what changed, not the tool's JSON
        // summary; any diff failure (bad args, unreadable file, …) falls through to the
        // plain output body so a broken diff can never break the block.
        if self.status == ToolStatus::Ok {
            let width = terminal_width();
            let diff_lines = match self.name.as_str() {
                "edit_file" => self
                    .args_json
                    .as_deref()
                    .and_then(|a| diff::render_edit_file(a, self.display_mode, width)),
                "write_file" => self
                    .args_json
                    .as_deref()
                    .and_then(|a| diff::render_write_file(a, self.display_mode, width)),
                _ => None,
            };
            if let Some(body) = diff_lines {
                lines.extend(body);
                return lines;
            }
        }
        let body: Vec<&str> = self.output.trim_end().lines().collect();
        let (limit, more) = match self.display_mode {
            DisplayMode::Collapsed => unreachable!("collapsed returns above"),
            DisplayMode::Truncated => (8, "lines"),
            DisplayMode::Expanded => (40, "more"),
        };
        let muted = Style::default().fg(theme().text_faint);
        for l in body.iter().take(limit) {
            lines.push(Line::from(Span::styled(format!("    {l}"), muted)));
        }
        if body.len() > limit {
            lines.push(Line::from(Span::styled(
                format!("    … +{} {more}", body.len() - limit),
                muted,
            )));
        }
        lines
    }
}

/// Default folding for a finished block. Read-only tools collapse to their title (they
/// are also the only aggregation candidates — see [`aggregate_line`]); write/command
/// tools and every failure open to Truncated so the operator sees what changed or why
/// it failed. Tools the registry doesn't know (`is_read_only == None`) are treated
/// conservatively as non-read-only.
pub fn default_display_mode(name: &str, is_read_only: Option<bool>, success: bool) -> DisplayMode {
    if !success {
        return DisplayMode::Truncated;
    }
    const WRITE_OR_COMMAND: &[&str] = &[
        "edit_file",
        "write_file",
        "execute_command",
        "execute_python",
        "spawn_subagent",
    ];
    if WRITE_OR_COMMAND.contains(&name) {
        return DisplayMode::Truncated;
    }
    match is_read_only {
        Some(true) => DisplayMode::Collapsed,
        _ => DisplayMode::Truncated,
    }
}

/// Suffix of a title line. `DiffStat` is rendered with green/red spans; everything
/// else is a single muted string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Suffix {
    None,
    Plain(String),
    DiffStat { added: usize, removed: usize },
}

impl Suffix {
    fn text(&self) -> String {
        match self {
            Suffix::None => String::new(),
            Suffix::Plain(s) => s.clone(),
            Suffix::DiffStat { added, removed } => format!("+{added}/-{removed}"),
        }
    }
}

/// Decomposed title: bold verb, accent-coloured target, muted suffix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerbTitle {
    pub verb: String,
    pub target: String,
    pub suffix: Suffix,
}

/// Map a tool name + args JSON to its title parts. Any parse failure or missing field
/// degrades to the bare tool name (never an error — this is a display concern only).
pub fn verb_title(name: &str, args_json: Option<&str>) -> VerbTitle {
    let args: Option<serde_json::Value> = args_json.and_then(|a| serde_json::from_str(a).ok());
    let get = |key: &str| args.as_ref()?.get(key)?.as_str().map(str::to_string);
    let get_i64 = |key: &str| args.as_ref()?.get(key)?.as_i64();

    match name {
        "read_file" => {
            let path = get("path").unwrap_or_default();
            let suffix = match (get_i64("offset"), get_i64("limit")) {
                (offset, Some(limit)) => {
                    let a = offset.unwrap_or(1);
                    Suffix::Plain(format!("({}-{})", a, a + limit - 1))
                }
                (Some(a), None) if a != 1 => Suffix::Plain(format!("(from {a})")),
                _ => Suffix::None,
            };
            VerbTitle {
                verb: "Read".into(),
                target: path,
                suffix,
            }
        }
        "edit_file" => {
            let path = get("path").unwrap_or_default();
            // Diffstat from the raw strings: removed = lines of old_string, added =
            // lines of new_string. A one-line change reads `+1/-1`.
            let removed = get("old_string").map(|s| s.lines().count()).unwrap_or(0);
            let added = get("new_string").map(|s| s.lines().count()).unwrap_or(0);
            VerbTitle {
                verb: "Edit".into(),
                target: path,
                suffix: Suffix::DiffStat { added, removed },
            }
        }
        "write_file" => {
            let path = get("path").unwrap_or_default();
            let n = get("content").map(|c| c.lines().count()).unwrap_or(0);
            VerbTitle {
                verb: "Write".into(),
                target: path,
                suffix: Suffix::Plain(format!("({n} lines)")),
            }
        }
        "execute_command" => VerbTitle {
            verb: "$".into(),
            target: clip(&get("command").unwrap_or_default(), 80),
            suffix: Suffix::None,
        },
        "execute_python" => {
            let first = get("code")
                .and_then(|c| c.lines().next().map(str::to_string))
                .unwrap_or_default();
            VerbTitle {
                verb: "Python".into(),
                target: format!("‹{}›", clip(&first, 60)),
                suffix: Suffix::None,
            }
        }
        "grep" => {
            let pattern = get("pattern").unwrap_or_default();
            let suffix = match get("path") {
                Some(p) if p != "." => Suffix::Plain(format!("in {p}")),
                _ => Suffix::None,
            };
            VerbTitle {
                verb: "Search".into(),
                target: format!("\"{}\"", clip(&pattern, 40)),
                suffix,
            }
        }
        "glob" => VerbTitle {
            verb: "Glob".into(),
            target: format!("\"{}\"", clip(&get("pattern").unwrap_or_default(), 40)),
            suffix: Suffix::None,
        },
        "http_request" => {
            let method = get("method").unwrap_or_else(|| "GET".into()).to_uppercase();
            VerbTitle {
                verb: method,
                target: clip(&get("url").unwrap_or_default(), 80),
                suffix: Suffix::None,
            }
        }
        "web_fetch" => VerbTitle {
            verb: "Fetch".into(),
            target: clip(&get("url").unwrap_or_default(), 80),
            suffix: Suffix::None,
        },
        "browser" => {
            let action = get("action").unwrap_or_default();
            let suffix = match get("url") {
                Some(u) => Suffix::Plain(clip(&u, 60)),
                None => Suffix::None,
            };
            VerbTitle {
                verb: "Browse".into(),
                target: action,
                suffix,
            }
        }
        "spawn_subagent" => {
            let desc = get("task").unwrap_or_default();
            VerbTitle {
                verb: "Subagent".into(),
                target: format!("\"{}\"", clip(&first_line_of(&desc), 50)),
                suffix: Suffix::None,
            }
        }
        // Unknown tool: bare name + the first scalar argument as a hint of what it does.
        _ => {
            let target = args
                .as_ref()
                .and_then(|v| v.as_object().cloned())
                .and_then(|obj| {
                    obj.values().find_map(|v| match v {
                        serde_json::Value::String(s) => Some(clip(s, 40)),
                        serde_json::Value::Number(n) => Some(n.to_string()),
                        serde_json::Value::Bool(b) => Some(b.to_string()),
                        _ => None,
                    })
                })
                .unwrap_or_default();
            VerbTitle {
                verb: name.to_string(),
                target,
                suffix: Suffix::None,
            }
        }
    }
}

/// Styled spans for the verb/target/suffix part of a title (shared by the scrollback
/// title line and any future live-region rendering).
fn title_spans(name: &str, args_json: Option<&str>) -> Vec<Span<'static>> {
    let t = verb_title(name, args_json);
    // `$` is a prompt marker, not a verb — keep it muted so the command stands out.
    let verb_style = if t.verb == "$" {
        Style::default().fg(theme().text_faint)
    } else {
        Style::default()
            .fg(theme().text)
            .add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![Span::styled(t.verb, verb_style)];
    if !t.target.is_empty() {
        spans.push(Span::raw(" ".to_string()));
        spans.push(Span::styled(t.target, Style::default().fg(theme().accent)));
    }
    match t.suffix {
        Suffix::None => {}
        Suffix::Plain(s) => {
            spans.push(Span::raw(" ".to_string()));
            spans.push(Span::styled(s, Style::default().fg(theme().text_faint)));
        }
        Suffix::DiffStat { added, removed } => {
            spans.push(Span::raw(" ".to_string()));
            spans.push(Span::styled(
                format!("+{added}"),
                Style::default().fg(theme().diff_insert_fg),
            ));
            spans.push(Span::styled(
                "/".to_string(),
                Style::default().fg(theme().text_faint),
            ));
            spans.push(Span::styled(
                format!("-{removed}"),
                Style::default().fg(theme().diff_delete_fg),
            ));
        }
    }
    spans
}

// ── verb-group aggregation ──

/// Render the buffered read-only blocks as scrollback lines: a single block renders
/// normally (its collapsed title line); two or more collapse into one aggregate line
/// like `Read 2 files · Searched 1 pattern`. Groups keep first-seen verb order.
pub fn aggregate_lines(blocks: &[ToolBlock]) -> Vec<Line<'static>> {
    match blocks {
        [] => Vec::new(),
        [single] => single.render(),
        many => {
            let mut groups: Vec<(String, usize)> = Vec::new();
            for b in many {
                let verb = verb_title(&b.name, b.args_json.as_deref()).verb;
                match groups.iter_mut().find(|(v, _)| *v == verb) {
                    Some((_, n)) => *n += 1,
                    None => groups.push((verb, 1)),
                }
            }
            let text = groups
                .iter()
                .map(|(verb, n)| group_label(verb, *n))
                .collect::<Vec<_>>()
                .join(" · ");
            vec![Line::from(Span::styled(
                format!("  {text}"),
                Style::default().fg(theme().text_muted),
            ))]
        }
    }
}

/// Aggregate phrase for one verb group, e.g. `Read 3 files`, `Searched 1 pattern`.
fn group_label(verb: &str, n: usize) -> String {
    let plural = |one: &str, many: &str| {
        if n == 1 {
            one.to_string()
        } else {
            many.to_string()
        }
    };
    match verb {
        "Read" => format!("Read {n} {}", plural("file", "files")),
        "Search" => format!("Searched {n} {}", plural("pattern", "patterns")),
        "Glob" => format!("Globbed {n} {}", plural("pattern", "patterns")),
        "Fetch" => format!("Fetched {n} {}", plural("page", "pages")),
        "Browse" => format!("Browsed {n} {}", plural("page", "pages")),
        other => format!("{other} ×{n}"),
    }
}

// ── running-state wave animation ──

/// Left-edge marker for the busy status line while tools run: a sine-wave brightness
/// ripple across three accent bars (TrueColor only — a quantized/16-color theme falls
/// back to the rotating spinner glyphs, which need no color at all).
pub fn wave_spans(step: usize) -> Vec<Span<'static>> {
    match theme().accent {
        Color::Rgb(r, g, b) => (0..3)
            .map(|i| {
                let phase = step as f64 * 0.7 + i as f64 * 0.9;
                // Keep a floor so the bar never fully disappears.
                let s = 0.35 + 0.65 * (0.5 + 0.5 * phase.sin());
                let scale = |v: u8| (v as f64 * s).round() as u8;
                Span::styled(
                    "▍".to_string(),
                    Style::default().fg(Color::Rgb(scale(r), scale(g), scale(b))),
                )
            })
            .collect(),
        _ => vec![Span::styled(
            glyphs().spinner[step % glyphs().spinner.len()].to_string(),
            Style::default().fg(theme().accent_busy),
        )],
    }
}

// ── small helpers ──

/// Terminal width for pre-truncating diff rows (they must not wrap — a wrapped band
/// would split across rows). Off-tty (unit tests) fall back to 80.
fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .max(1)
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() > max {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    } else {
        s.to_string()
    }
}

fn first_line_of(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── verb_title mapping ──

    #[test]
    fn read_file_title_with_range_suffix() {
        let t = verb_title(
            "read_file",
            Some(r#"{"path":"src/main.rs","offset":10,"limit":20}"#),
        );
        assert_eq!(t.verb, "Read");
        assert_eq!(t.target, "src/main.rs");
        assert_eq!(t.suffix, Suffix::Plain("(10-29)".into()));
    }

    #[test]
    fn read_file_title_defaults_offset_to_1() {
        let t = verb_title("read_file", Some(r#"{"path":"a.rs","limit":5}"#));
        assert_eq!(t.suffix, Suffix::Plain("(1-5)".into()));
        let t = verb_title("read_file", Some(r#"{"path":"a.rs"}"#));
        assert_eq!(t.suffix, Suffix::None);
    }

    #[test]
    fn edit_file_title_computes_diffstat() {
        let args = r#"{"path":"x.rs","old_string":"a\nb\nc","new_string":"d"}"#;
        let t = verb_title("edit_file", Some(args));
        assert_eq!(t.verb, "Edit");
        assert_eq!(t.target, "x.rs");
        assert_eq!(
            t.suffix,
            Suffix::DiffStat {
                added: 1,
                removed: 3
            }
        );
    }

    #[test]
    fn write_file_title_counts_lines() {
        let t = verb_title(
            "write_file",
            Some(r#"{"path":"o.txt","content":"l1\nl2\nl3"}"#),
        );
        assert_eq!(t.verb, "Write");
        assert_eq!(t.suffix, Suffix::Plain("(3 lines)".into()));
    }

    #[test]
    fn execute_command_title_uses_dollar_verb() {
        let t = verb_title("execute_command", Some(r#"{"command":"nmap -sV target"}"#));
        assert_eq!(t.verb, "$");
        assert_eq!(t.target, "nmap -sV target");
    }

    #[test]
    fn python_title_shows_first_line() {
        let t = verb_title("execute_python", Some(r#"{"code":"import os\nprint(1)"}"#));
        assert_eq!(t.verb, "Python");
        assert_eq!(t.target, "‹import os›");
    }

    #[test]
    fn grep_and_glob_titles_quote_the_pattern() {
        let t = verb_title("grep", Some(r#"{"pattern":"passw.","path":"src/"}"#));
        assert_eq!(t.verb, "Search");
        assert_eq!(t.target, "\"passw.\"");
        assert_eq!(t.suffix, Suffix::Plain("in src/".into()));
        let t = verb_title("glob", Some(r#"{"pattern":"**/*.rs"}"#));
        assert_eq!(t.verb, "Glob");
        assert_eq!(t.target, "\"**/*.rs\"");
    }

    #[test]
    fn http_and_fetch_titles() {
        let t = verb_title(
            "http_request",
            Some(r#"{"url":"http://t/api","method":"post"}"#),
        );
        assert_eq!(t.verb, "POST");
        assert_eq!(t.target, "http://t/api");
        let t = verb_title("http_request", Some(r#"{"url":"http://t"}"#));
        assert_eq!(t.verb, "GET", "method defaults to GET");
        let t = verb_title("web_fetch", Some(r#"{"url":"http://x"}"#));
        assert_eq!(t.verb, "Fetch");
    }

    #[test]
    fn browser_and_subagent_titles() {
        let t = verb_title("browser", Some(r#"{"action":"navigate","url":"http://t"}"#));
        assert_eq!(t.verb, "Browse");
        assert_eq!(t.target, "navigate");
        let t = verb_title(
            "spawn_subagent",
            Some(r#"{"task":"enumerate subdomains\nfast"}"#),
        );
        assert_eq!(t.verb, "Subagent");
        assert_eq!(t.target, "\"enumerate subdomains\"");
    }

    #[test]
    fn unknown_tool_falls_back_to_name_and_first_scalar() {
        let t = verb_title("nmap_scan", Some(r#"{"target":"10.0.0.1"}"#));
        assert_eq!(t.verb, "nmap_scan");
        assert_eq!(t.target, "10.0.0.1");
    }

    #[test]
    fn bad_or_missing_args_degrade_to_bare_name() {
        let t = verb_title("read_file", Some("not json {"));
        assert_eq!(t.verb, "Read");
        assert_eq!(t.target, "");
        let t = verb_title("whatever", None);
        assert_eq!(t.verb, "whatever");
        assert_eq!(t.target, "");
    }

    // ── display modes ──

    #[test]
    fn default_modes_follow_read_only_and_success() {
        assert_eq!(
            default_display_mode("read_file", Some(true), true),
            DisplayMode::Collapsed
        );
        assert_eq!(
            default_display_mode("edit_file", Some(false), true),
            DisplayMode::Truncated
        );
        assert_eq!(
            default_display_mode("read_file", Some(true), false),
            DisplayMode::Truncated,
            "failures always open so the error is visible"
        );
        assert_eq!(
            default_display_mode("mcp_mystery", None, true),
            DisplayMode::Truncated,
            "unknown tools are treated as non-read-only"
        );
    }

    fn finished_block(
        name: &str,
        args: Option<&str>,
        output: &str,
        mode: DisplayMode,
    ) -> ToolBlock {
        ToolBlock {
            call_id: Some("c1".into()),
            name: name.into(),
            args_json: args.map(str::to_string),
            status: ToolStatus::Ok,
            output: output.into(),
            display_mode: mode,
        }
    }

    #[test]
    fn collapsed_renders_title_only() {
        let b = finished_block(
            "read_file",
            Some(r#"{"path":"a.rs"}"#),
            "body",
            DisplayMode::Collapsed,
        );
        let lines = b.render();
        assert_eq!(lines.len(), 1);
        assert!(plain(&lines)[0].contains("✓ Read a.rs"));
    }

    #[test]
    fn truncated_shows_eight_lines_plus_overflow_marker() {
        let out = (1..=20).map(|i| format!("line{i}\n")).collect::<String>();
        let b = finished_block(
            "edit_file",
            Some(r#"{"path":"x"}"#),
            &out,
            DisplayMode::Truncated,
        );
        let text = plain(&b.render());
        assert_eq!(text.len(), 1 + 8 + 1);
        assert!(text[1].contains("line1"));
        assert!(text[8].contains("line8"));
        assert_eq!(text[9].trim(), "… +12 lines");
    }

    #[test]
    fn expanded_caps_at_forty_lines() {
        let out = (1..=50).map(|i| format!("l{i}\n")).collect::<String>();
        let b = finished_block("x", None, &out, DisplayMode::Expanded);
        let text = plain(&b.render());
        assert_eq!(text.len(), 1 + 40 + 1);
        assert_eq!(text[41].trim(), "… +10 more");
    }

    #[test]
    fn title_line_marks_failures_red_cross() {
        let mut b = finished_block(
            "grep",
            Some(r#"{"pattern":"x"}"#),
            "boom",
            DisplayMode::Truncated,
        );
        b.status = ToolStatus::Err;
        let text = plain(&b.render());
        assert!(text[0].contains("✗ Search \"x\""));
    }

    // ── diff body integration ──

    #[test]
    fn successful_edit_replaces_json_output_with_diff() {
        let args = r#"{"path":"src/x.unknownext","old_string":"let a = 1;\n","new_string":"let a = 2;\n"}"#;
        let out = r#"{"path":"src/x.unknownext","replacements":1}"#;
        let b = finished_block("edit_file", Some(args), out, DisplayMode::Truncated);
        let lines = b.render();
        let text = plain(&lines);
        assert!(text[0].contains("✓ Edit src/x.unknownext +1/-1"));
        assert!(
            text.iter().any(|l| l.contains("let a = 1;")),
            "delete row shown"
        );
        assert!(
            text.iter().any(|l| l.contains("let a = 2;")),
            "insert row shown"
        );
        assert!(
            !text.iter().any(|l| l.contains("replacements")),
            "raw JSON hidden"
        );
        // Insert row carries the theme's insert band.
        let ins = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("let a = 2;")))
            .unwrap();
        assert_eq!(
            ins.spans.last().unwrap().style.bg,
            Some(theme().diff_insert_bg)
        );
    }

    #[test]
    fn failed_edit_keeps_the_error_output_body() {
        let args = r#"{"path":"x","old_string":"a","new_string":"b"}"#;
        let mut b = finished_block(
            "edit_file",
            Some(args),
            "old_string not found in 'x'",
            DisplayMode::Truncated,
        );
        b.status = ToolStatus::Err;
        let text = plain(&b.render());
        assert!(
            text.iter().any(|l| l.contains("old_string not found")),
            "got {text:?}"
        );
    }

    #[test]
    fn successful_write_renders_all_insert_body() {
        let args = r#"{"path":"out.unknownext","content":"first\nsecond\n"}"#;
        let b = finished_block(
            "write_file",
            Some(args),
            r#"{"bytes_written":13}"#,
            DisplayMode::Truncated,
        );
        let lines = b.render();
        let text = plain(&lines);
        assert_eq!(lines.len(), 1 + 2, "title + two insert rows");
        assert!(text[1].starts_with("  1 first"), "got {:?}", text[1]);
        assert!(text[2].starts_with("  2 second"), "got {:?}", text[2]);
        // Unknown extension → flat insert foreground (no syntect).
        assert_eq!(
            lines[1].spans.last().unwrap().style.fg,
            Some(theme().diff_insert_fg)
        );
    }

    #[test]
    fn edit_with_bad_args_falls_back_to_output_body() {
        // Args lacking old/new strings can't build a diff → the JSON output shows.
        let b = finished_block(
            "edit_file",
            Some(r#"{"path":"x"}"#),
            "summary body",
            DisplayMode::Truncated,
        );
        let text = plain(&b.render());
        assert!(text.iter().any(|l| l.contains("summary body")));
    }

    // ── aggregation ──

    fn read_block(path: &str) -> ToolBlock {
        finished_block(
            "read_file",
            Some(&format!(r#"{{"path":"{path}"}}"#)),
            "",
            DisplayMode::Collapsed,
        )
    }

    #[test]
    fn single_pending_block_renders_normally() {
        let lines = aggregate_lines(&[read_block("a.rs")]);
        assert_eq!(lines.len(), 1);
        assert!(
            plain(&lines)[0].contains("Read a.rs"),
            "no fake aggregate for one block"
        );
    }

    #[test]
    fn three_reads_aggregate_to_one_line() {
        let blocks = [read_block("a"), read_block("b"), read_block("c")];
        let lines = aggregate_lines(&blocks);
        assert_eq!(plain(&lines), vec!["  Read 3 files"]);
    }

    #[test]
    fn mixed_verbs_join_with_dot_separator() {
        let blocks = [
            read_block("a"),
            read_block("b"),
            finished_block(
                "grep",
                Some(r#"{"pattern":"p"}"#),
                "",
                DisplayMode::Collapsed,
            ),
        ];
        assert_eq!(
            plain(&aggregate_lines(&blocks)),
            vec!["  Read 2 files · Searched 1 pattern"]
        );
    }

    #[test]
    fn wave_renders_three_accent_bars_on_truecolor_theme() {
        // Tests never run `init_theme()`, so `theme()` is the TrueColor fallback and the
        // wave path (not the spinner-glyph fallback) is what we exercise here.
        let spans = wave_spans(3);
        assert_eq!(spans.len(), 3);
        for s in spans {
            assert_eq!(s.content.as_ref(), "▍");
            assert!(matches!(s.style.fg, Some(Color::Rgb(..))));
        }
    }
}
