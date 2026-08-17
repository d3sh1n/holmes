//! Semantic color theme + terminal capability degradation.
//!
//! Colors in the UI are never hardcoded: widgets read semantic fields off `theme()`
//! (e.g. `text_faint`, `accent`, `failure`). At startup `init_theme()` detects the
//! terminal's color level and *quantizes* the built-in TrueColor theme once — mapping
//! every `Color::Rgb` down to the closest `Color::Indexed` (256-color) or a fixed ANSI
//! slot (16-color) — so the rest of the code can stay capability-agnostic. Glyphs get
//! the same treatment: fancy Unicode (❯, braille spinner) degrades to ASCII on
//! limited terminals.

use std::sync::OnceLock;

use ratatui::style::Color;

/// Flat set of semantic colors the UI draws with. Every field is a role, not a value —
/// which concrete color a role gets depends on the active theme and color level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Primary body text.
    pub text: Color,
    /// Secondary text (tool names, metadata).
    pub text_muted: Color,
    /// Comments / hint rows (replaces ad-hoc `Color::DarkGray`).
    pub text_faint: Color,
    /// Holmes prefix / idle accents.
    pub accent: Color,
    /// Busy-state accent (spinner while a turn runs).
    pub accent_busy: Color,
    /// "You" prefix for user messages.
    pub accent_user: Color,
    pub success: Color,
    pub failure: Color,
    pub warning: Color,
    /// Evidence / finding highlights.
    pub evidence: Color,
    /// Slash-command menu entries.
    pub menu_fg: Color,
    pub menu_sel_fg: Color,
    pub menu_sel_bg: Color,
    /// Diff palette: dark tinted backgrounds with readable foregrounds.
    pub diff_insert_bg: Color,
    pub diff_delete_bg: Color,
    pub diff_insert_fg: Color,
    pub diff_delete_fg: Color,
    pub diff_context_fg: Color,
    pub diff_gutter_fg: Color,
    /// Input-box border, idle vs busy.
    pub border: Color,
    pub border_busy: Color,
}

impl Theme {
    /// Built-in dark theme ("Holmes night"), all TrueColor. Quantized down at startup
    /// when the terminal can't display 24-bit color.
    pub const fn holmes_night() -> Theme {
        Theme {
            text: Color::Rgb(0xd4, 0xd4, 0xd4),
            text_muted: Color::Rgb(0x9d, 0x9d, 0x9d),
            text_faint: Color::Rgb(0x6a, 0x6a, 0x6a),
            accent: Color::Rgb(0x4e, 0xc9, 0xb0),
            accent_busy: Color::Rgb(0xdc, 0xdc, 0xaa),
            accent_user: Color::Rgb(0x6a, 0x99, 0x55),
            success: Color::Rgb(0x6a, 0x99, 0x55),
            failure: Color::Rgb(0xf1, 0x4c, 0x4c),
            warning: Color::Rgb(0xdc, 0xdc, 0xaa),
            evidence: Color::Rgb(0xc5, 0x86, 0xc0),
            menu_fg: Color::Rgb(0x4e, 0xc9, 0xb0),
            menu_sel_fg: Color::Rgb(0x1b, 0x1b, 0x1b),
            menu_sel_bg: Color::Rgb(0x4e, 0xc9, 0xb0),
            diff_insert_bg: Color::Rgb(0x0f, 0x41, 0x14),
            diff_delete_bg: Color::Rgb(0x55, 0x0f, 0x14),
            diff_insert_fg: Color::Rgb(0x81, 0xb8, 0x8a),
            diff_delete_fg: Color::Rgb(0xc9, 0x72, 0x72),
            diff_context_fg: Color::Rgb(0x9d, 0x9d, 0x9d),
            diff_gutter_fg: Color::Rgb(0x5a, 0x5a, 0x5a),
            border: Color::Rgb(0x4e, 0xc9, 0xb0),
            border_busy: Color::Rgb(0xdc, 0xdc, 0xaa),
        }
    }

    /// Adapt this theme to the terminal's color level. Called once at startup; a pure
    /// function so the mapping is unit-testable.
    pub fn quantize(self, level: ColorLevel) -> Theme {
        match level {
            ColorLevel::TrueColor => self,
            ColorLevel::Ansi256 => self.map_colors(rgb_to_indexed),
            // 16 colors can't express the palette — pin each role to the closest ANSI
            // slot (this mirrors the colors the UI used to hardcode).
            ColorLevel::Ansi16 => Theme::ansi16(),
            // No color at all: Reset everywhere lets the terminal default show through.
            ColorLevel::None => Theme::no_color(),
        }
    }

    /// Every field mapped through `f` (used by the 256-color quantizer).
    fn map_colors(self, f: fn(Color) -> Color) -> Theme {
        Theme {
            text: f(self.text),
            text_muted: f(self.text_muted),
            text_faint: f(self.text_faint),
            accent: f(self.accent),
            accent_busy: f(self.accent_busy),
            accent_user: f(self.accent_user),
            success: f(self.success),
            failure: f(self.failure),
            warning: f(self.warning),
            evidence: f(self.evidence),
            menu_fg: f(self.menu_fg),
            menu_sel_fg: f(self.menu_sel_fg),
            menu_sel_bg: f(self.menu_sel_bg),
            diff_insert_bg: f(self.diff_insert_bg),
            diff_delete_bg: f(self.diff_delete_bg),
            diff_insert_fg: f(self.diff_insert_fg),
            diff_delete_fg: f(self.diff_delete_fg),
            diff_context_fg: f(self.diff_context_fg),
            diff_gutter_fg: f(self.diff_gutter_fg),
            border: f(self.border),
            border_busy: f(self.border_busy),
        }
    }

    /// Semantic roles pinned to ANSI-16 slots.
    fn ansi16() -> Theme {
        Theme {
            text: Color::Reset,
            text_muted: Color::Gray,
            text_faint: Color::DarkGray,
            accent: Color::Cyan,
            accent_busy: Color::Yellow,
            accent_user: Color::Green,
            success: Color::Green,
            failure: Color::Red,
            warning: Color::Yellow,
            evidence: Color::Magenta,
            menu_fg: Color::Cyan,
            menu_sel_fg: Color::Black,
            menu_sel_bg: Color::Cyan,
            // No room for tinted backgrounds at 16 colors — signal via foreground only.
            diff_insert_bg: Color::Reset,
            diff_delete_bg: Color::Reset,
            diff_insert_fg: Color::Green,
            diff_delete_fg: Color::Red,
            diff_context_fg: Color::DarkGray,
            diff_gutter_fg: Color::DarkGray,
            border: Color::Cyan,
            border_busy: Color::Yellow,
        }
    }

    fn no_color() -> Theme {
        Theme {
            text: Color::Reset,
            text_muted: Color::Reset,
            text_faint: Color::Reset,
            accent: Color::Reset,
            accent_busy: Color::Reset,
            accent_user: Color::Reset,
            success: Color::Reset,
            failure: Color::Reset,
            warning: Color::Reset,
            evidence: Color::Reset,
            menu_fg: Color::Reset,
            menu_sel_fg: Color::Reset,
            menu_sel_bg: Color::Reset,
            diff_insert_bg: Color::Reset,
            diff_delete_bg: Color::Reset,
            diff_insert_fg: Color::Reset,
            diff_delete_fg: Color::Reset,
            diff_context_fg: Color::Reset,
            diff_gutter_fg: Color::Reset,
            border: Color::Reset,
            border_busy: Color::Reset,
        }
    }
}

/// How much color the terminal can display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorLevel {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

/// Detect the color level from this process's environment.
pub fn detect_color_level() -> ColorLevel {
    color_level_from_env(|k| std::env::var(k).ok())
}

/// Pure, env-injectable detection (tests pass a fake getter). Precedence:
/// `NO_COLOR` wins; then `COLORTERM` truecolor; then a `256color` TERM; a missing or
/// `dumb` TERM means no reliable color; anything else gets the basic 16.
pub fn color_level_from_env(get: impl Fn(&str) -> Option<String>) -> ColorLevel {
    if get("NO_COLOR").is_some() {
        return ColorLevel::None;
    }
    if let Some(ct) = get("COLORTERM") {
        let ct = ct.to_lowercase();
        if ct == "truecolor" || ct == "24bit" {
            return ColorLevel::TrueColor;
        }
    }
    match get("TERM").map(|t| t.to_lowercase()) {
        Some(t) if t.contains("256color") => ColorLevel::Ansi256,
        Some(t) if t == "dumb" || t.is_empty() => ColorLevel::None,
        Some(_) => ColorLevel::Ansi16,
        None => ColorLevel::None,
    }
}

/// Map a color to the 256-color palette: `Rgb` snaps to the nearest entry of the
/// 6×6×6 color cube or the 24-step gray ramp (whichever is closer); everything else
/// (already Indexed or ANSI) passes through.
fn rgb_to_indexed(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    // Cube levels are not evenly spaced — snap each channel to the nearest one.
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let snap = |v: u8| -> (u8, u8) {
        let mut best = 0usize;
        for (i, &l) in LEVELS.iter().enumerate() {
            if (v as i32 - l as i32).abs() < (v as i32 - LEVELS[best] as i32).abs() {
                best = i;
            }
        }
        (best as u8, LEVELS[best])
    };
    let (ri, rv) = snap(r);
    let (gi, gv) = snap(g);
    let (bi, bv) = snap(b);
    let cube_dist = (r as i32 - rv as i32).pow(2)
        + (g as i32 - gv as i32).pow(2)
        + (b as i32 - bv as i32).pow(2);
    // Gray ramp: entries 232..=255 are 8 + 10·n.
    let avg = (r as u16 + g as u16 + b as u16) / 3;
    let gray_idx = avg.saturating_sub(4).min(238) / 10;
    let gray_idx = gray_idx.min(23) as u8;
    let gray_v = 8 + 10 * gray_idx as u16;
    let gray_dist = 3 * (avg as i32 - gray_v as i32).pow(2);
    let idx = if gray_dist < cube_dist {
        232 + gray_idx
    } else {
        16 + 36 * ri + 6 * gi + bi
    };
    Color::Indexed(idx)
}

/// Text decorations that degrade alongside color: on limited terminals the braille
/// spinner, ❯ prompt and box-drawing chars render as tofu or noise, so fall back to
/// plain ASCII.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyphs {
    pub prompt: &'static str,
    pub spinner: &'static [&'static str],
    pub blockquote: &'static str,
    pub ellipsis: &'static str,
    /// Shown in place of the spinner while the agent is paused on an approval prompt.
    pub pause: &'static str,
}

const SPINNER_BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_ASCII: &[&str] = &["|", "/", "-", "\\"];

impl Glyphs {
    pub fn for_level(level: ColorLevel) -> Glyphs {
        match level {
            ColorLevel::TrueColor | ColorLevel::Ansi256 => Glyphs {
                prompt: "❯ ",
                spinner: SPINNER_BRAILLE,
                blockquote: "│",
                ellipsis: "…",
                pause: "⏸",
            },
            ColorLevel::Ansi16 | ColorLevel::None => Glyphs {
                prompt: "> ",
                spinner: SPINNER_ASCII,
                blockquote: "|",
                ellipsis: "...",
                pause: "||",
            },
        }
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();
static GLYPHS: OnceLock<Glyphs> = OnceLock::new();
static COLOR_LEVEL: OnceLock<ColorLevel> = OnceLock::new();

/// Detect capabilities once and install the quantized theme + glyphs. Idempotent —
/// later calls keep the first installation (OnceLock semantics).
pub fn init_theme() {
    let level = detect_color_level();
    let _ = COLOR_LEVEL.set(level);
    let _ = THEME.set(Theme::holmes_night().quantize(level));
    let _ = GLYPHS.set(Glyphs::for_level(level));
}

/// The active theme. Falls back to the raw TrueColor theme when `init_theme()` never
/// ran (unit tests, library use), so callers never need to care.
pub fn theme() -> &'static Theme {
    static FALLBACK: Theme = Theme::holmes_night();
    THEME.get().unwrap_or(&FALLBACK)
}

/// The active glyph set (same fallback policy as `theme()`).
pub fn glyphs() -> &'static Glyphs {
    static FALLBACK: Glyphs = Glyphs {
        prompt: "❯ ",
        spinner: SPINNER_BRAILLE,
        blockquote: "│",
        ellipsis: "…",
        pause: "⏸",
    };
    GLYPHS.get().unwrap_or(&FALLBACK)
}

/// The detected color level. Syntax highlighting keys off this (syntect themes are
/// TrueColor palettes that can't be quantized per-token without drifting from the
/// semantic theme), so it must survive quantization as its own value. Falls back to
/// TrueColor when `init_theme()` never ran, matching the `theme()` fallback.
pub fn color_level() -> ColorLevel {
    COLOR_LEVEL.get().copied().unwrap_or(ColorLevel::TrueColor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detects_truecolor_from_colorterm() {
        let level = color_level_from_env(env(&[
            ("COLORTERM", "truecolor"),
            ("TERM", "xterm-256color"),
        ]));
        assert_eq!(level, ColorLevel::TrueColor);
    }

    #[test]
    fn detects_256_from_term() {
        let level = color_level_from_env(env(&[("TERM", "xterm-256color")]));
        assert_eq!(level, ColorLevel::Ansi256);
    }

    #[test]
    fn no_color_wins_over_everything() {
        let level = color_level_from_env(env(&[
            ("NO_COLOR", "1"),
            ("COLORTERM", "truecolor"),
            ("TERM", "xterm-256color"),
        ]));
        assert_eq!(level, ColorLevel::None);
    }

    #[test]
    fn dumb_or_missing_term_means_no_color() {
        assert_eq!(
            color_level_from_env(env(&[("TERM", "dumb")])),
            ColorLevel::None
        );
        assert_eq!(color_level_from_env(env(&[])), ColorLevel::None);
    }

    #[test]
    fn plain_term_gets_ansi16() {
        assert_eq!(
            color_level_from_env(env(&[("TERM", "xterm")])),
            ColorLevel::Ansi16
        );
    }

    #[test]
    fn quantize_truecolor_is_identity() {
        let t = Theme::holmes_night();
        assert_eq!(t.quantize(ColorLevel::TrueColor), t);
    }

    #[test]
    fn quantize_256_maps_all_rgb_to_indexed() {
        let t = Theme::holmes_night().quantize(ColorLevel::Ansi256);
        let all = [
            t.text,
            t.text_muted,
            t.text_faint,
            t.accent,
            t.accent_busy,
            t.accent_user,
            t.success,
            t.failure,
            t.warning,
            t.evidence,
            t.menu_fg,
            t.menu_sel_fg,
            t.menu_sel_bg,
            t.diff_insert_bg,
            t.diff_delete_bg,
            t.diff_insert_fg,
            t.diff_delete_fg,
            t.diff_context_fg,
            t.diff_gutter_fg,
            t.border,
            t.border_busy,
        ];
        for c in all {
            assert!(
                matches!(c, Color::Indexed(_)),
                "expected Indexed, got {c:?}"
            );
        }
    }

    #[test]
    fn quantize_ansi16_pins_semantic_slots() {
        let t = Theme::holmes_night().quantize(ColorLevel::Ansi16);
        assert_eq!(t.accent, Color::Cyan);
        assert_eq!(t.failure, Color::Red);
        assert_eq!(t.text_faint, Color::DarkGray);
        assert_eq!(t.menu_sel_bg, Color::Cyan);
    }

    #[test]
    fn quantize_none_resets_everything() {
        let t = Theme::holmes_night().quantize(ColorLevel::None);
        assert_eq!(t.accent, Color::Reset);
        assert_eq!(t.diff_insert_bg, Color::Reset);
    }

    #[test]
    fn rgb_to_indexed_snaps_pure_primaries_into_cube() {
        // Pure red is cube entry (5,0,0) = 196; pure gray lands on the ramp.
        assert_eq!(rgb_to_indexed(Color::Rgb(255, 0, 0)), Color::Indexed(196));
        let gray = rgb_to_indexed(Color::Rgb(128, 128, 128));
        assert!(matches!(gray, Color::Indexed(i) if (232..=255).contains(&i)));
    }

    #[test]
    fn glyphs_degrade_on_limited_terminals() {
        assert_eq!(Glyphs::for_level(ColorLevel::TrueColor).prompt, "❯ ");
        assert_eq!(Glyphs::for_level(ColorLevel::Ansi256).spinner.len(), 10);
        assert_eq!(Glyphs::for_level(ColorLevel::Ansi16).prompt, "> ");
        assert_eq!(Glyphs::for_level(ColorLevel::None).blockquote, "|");
        assert_eq!(Glyphs::for_level(ColorLevel::Ansi16).ellipsis, "...");
    }

    #[test]
    fn theme_fallback_works_without_init() {
        // `theme()` must be callable in tests that never ran `init_theme()`.
        let _ = theme().accent;
        let _ = glyphs().prompt;
    }
}
