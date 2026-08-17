//! Syntax highlighting for diff bodies via syntect.
//!
//! Grok highlights hunks only (not whole files) and treats each side of a hunk as its
//! own fragment; the *splitting* lives in `diff.rs`, this module only colors one
//! contiguous fragment at a time. Two degradation levels, both returning `None` so the
//! caller falls back to the theme's flat per-side foreground colors:
//!
//! - unknown extension → no syntax definition, nothing to highlight;
//! - 16-color / no-color terminals → highlighting disabled entirely: syntect themes are
//!   TrueColor palettes that can't be quantized per-token without drifting from the
//!   semantic theme, so low-color terminals get flat red/green rows instead (grok's
//!   16-color path does the same).

use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme as SyntectTheme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::ui::theme::{color_level, ColorLevel};

/// `load_defaults_*` uses syntect's embedded binary dumps, so this works in tests and
/// on machines with no external syntax/theme files.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static DARK_THEME: LazyLock<SyntectTheme> = LazyLock::new(|| {
    let themes = ThemeSet::load_defaults();
    themes
        .themes
        .get("base16-ocean.dark")
        .cloned()
        .or_else(|| themes.themes.values().next().cloned())
        .expect("syntect's embedded theme dump is never empty")
});

/// Whether token-level highlighting is worthwhile on this terminal. At Ansi256 the
/// theme itself already quantizes, and syntect's RGB values are close enough that
/// xterm's own 256 mapping reads fine; below that the palette can't express it.
pub fn highlighting_active() -> bool {
    matches!(color_level(), ColorLevel::Ansi256 | ColorLevel::TrueColor)
}

/// Highlight `lines` as one contiguous fragment of `path`'s language, returning
/// ratatui-styled tokens per input line. `None` when highlighting is off or the
/// extension is unknown — callers treat `None` as "render the line flat".
pub fn highlight_lines(path: &str, lines: &[&str]) -> Option<Vec<Vec<(Style, String)>>> {
    highlight_with(path, lines, highlighting_active())
}

/// Highlight a markdown code-block body by its fence language token (e.g. `rust`,
/// `py`). Same contract as [`highlight_lines`]: `None` = render flat.
pub fn highlight_code(lang_token: &str, lines: &[&str]) -> Option<Vec<Vec<(Style, String)>>> {
    if !highlighting_active() || lines.is_empty() {
        return None;
    }
    let syntax = SYNTAXES.find_syntax_by_token(lang_token)?;
    Some(highlight_syntax(syntax, lines))
}

/// The injectable core of [`highlight_lines`] (tests toggle `active` directly instead
/// of mutating the process-global color level).
fn highlight_with(path: &str, lines: &[&str], active: bool) -> Option<Vec<Vec<(Style, String)>>> {
    if !active || lines.is_empty() {
        return None;
    }
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    let syntax = SYNTAXES.find_syntax_by_extension(ext)?;
    Some(highlight_syntax(syntax, lines))
}

/// Highlight one contiguous fragment with a resolved syntax.
fn highlight_syntax(
    syntax: &syntect::parsing::SyntaxReference,
    lines: &[&str],
) -> Vec<Vec<(Style, String)>> {
    let mut hl = HighlightLines::new(syntax, &DARK_THEME);
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        // `load_defaults_newlines` tokenizes per line *with* the trailing newline, so
        // re-attach it; the newline is stripped back off the last token below.
        let with_nl = format!("{line}\n");
        match hl.highlight_line(&with_nl, &SYNTAXES) {
            Ok(regions) => {
                let mut tokens: Vec<(Style, String)> = Vec::new();
                for (st, text) in regions {
                    let text = text.strip_suffix('\n').unwrap_or(text);
                    if text.is_empty() {
                        continue;
                    }
                    tokens.push((convert_style(st), text.to_string()));
                }
                out.push(tokens);
            }
            // A parse failure on one line must not blank the hunk — emit it unstyled
            // (the caller's per-side fallback color applies).
            Err(_) => out.push(vec![(Style::default(), line.to_string())]),
        }
    }
    out
}

/// syntect TrueColor → ratatui style. The token background is dropped on purpose:
/// diff rows paint their own insert/delete band behind the tokens.
fn convert_style(st: syntect::highlighting::Style) -> Style {
    let fg = Color::Rgb(st.foreground.r, st.foreground.g, st.foreground.b);
    let mut mods = Modifier::empty();
    if st.font_style.contains(FontStyle::BOLD) {
        mods |= Modifier::BOLD;
    }
    if st.font_style.contains(FontStyle::ITALIC) {
        mods |= Modifier::ITALIC;
    }
    if st.font_style.contains(FontStyle::UNDERLINE) {
        mods |= Modifier::UNDERLINED;
    }
    Style::default().fg(fg).add_modifier(mods)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extension_produces_distinct_token_colors() {
        let lines = ["fn main() {", "    let s = \"hi\";"];
        let out = highlight_with("src/main.rs", &lines, true).expect("rs is a known syntax");
        assert_eq!(out.len(), 2);
        // `fn` (keyword) and the plain text around it must not share one flat color.
        let fgs: Vec<Option<Color>> = out
            .iter()
            .flat_map(|line| line.iter().map(|(st, _)| st.fg))
            .collect();
        let distinct: std::collections::HashSet<_> = fgs.iter().collect();
        assert!(
            distinct.len() >= 2,
            "expected real highlighting, got {fgs:?}"
        );
    }

    #[test]
    fn unknown_extension_falls_back_to_none() {
        assert!(highlight_with("x.zzzqqq", &["hello"], true).is_none());
        assert!(highlight_with("no_extension", &["hello"], true).is_none());
    }

    #[test]
    fn inactive_highlighting_returns_none() {
        // The 16-color degradation path: same input, highlighting switched off.
        assert!(highlight_with("src/main.rs", &["fn main() {}"], false).is_none());
    }

    #[test]
    fn empty_input_returns_none() {
        assert!(highlight_with("a.rs", &[], true).is_none());
    }
}
