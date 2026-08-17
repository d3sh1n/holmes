//! Dynamic live-region height for the inline viewport.
//!
//! ratatui 0.30 fixes the `Viewport::Inline` height at `Terminal` creation — there is
//! no setter. Growing the live region (e.g. to show a permission card above the input
//! box) therefore means rebuilding the `Terminal` against the same stdout. Rebuilds
//! re-run the cursor-position query, which is exactly what fails under some tmux/SSH
//! setups, so every rebuild has a fallback: on failure we rebuild at the base height
//! and permanently degrade to fixed-height mode (`LiveRegion::can_resize = false`).

use std::io;

use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::inline_ui::Backend;

/// Live-region height with no card: bordered input box (3) + status line (1).
pub const BASE_HEIGHT: u16 = 4;

/// Hard cap so an expanded card can never eat the whole screen on a short terminal.
pub const MAX_HEIGHT: u16 = 24;

/// Tracks the live region's current height and whether the terminal tolerated a
/// rebuild. Layout is always `[card rows (optional)] + [input 3] + [status 1]`.
pub struct LiveRegion {
    pub height: u16,
    pub can_resize: bool,
}

impl Default for LiveRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveRegion {
    pub fn new() -> Self {
        Self {
            height: BASE_HEIGHT,
            can_resize: true,
        }
    }

    /// Number of rows above the input box currently reserved for a card.
    pub fn card_rows(&self) -> u16 {
        self.height.saturating_sub(BASE_HEIGHT)
    }

    /// Resize the live region to `BASE_HEIGHT + extra_rows`. No-ops when the height
    /// already matches; on rebuild failure degrades to fixed-height mode (the caller
    /// then renders cards in compact form inside the base 4 rows).
    pub fn set_extra_rows(&mut self, terminal: &mut Terminal<Backend>, extra_rows: u16) {
        let want = (BASE_HEIGHT + extra_rows).min(MAX_HEIGHT);
        if want == self.height {
            return;
        }
        if !self.can_resize {
            return;
        }
        match set_live_height(terminal, want) {
            Ok(()) => self.height = want,
            Err(_) => {
                // This terminal can't rebuild its inline viewport — never try again.
                self.can_resize = false;
            }
        }
    }
}

/// Rebuild `terminal` with an inline viewport of `new_h` rows. The old terminal is
/// cleared first so no stale viewport rows linger. On failure the terminal is rebuilt
/// at `BASE_HEIGHT` (best effort) and the error is returned so the caller can degrade.
pub fn set_live_height(terminal: &mut Terminal<Backend>, new_h: u16) -> io::Result<()> {
    // Clear the old viewport area before dropping it, or its rows stay on screen.
    terminal.clear()?;
    // Swap in a placeholder so `terminal` stays valid across the fallible rebuild.
    let placeholder = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let old = std::mem::replace(terminal, placeholder);
    drop(old);
    match rebuild(new_h) {
        Ok(t) => {
            *terminal = t;
            Ok(())
        }
        Err(err) => {
            // Fall back to the base height; if even that fails, the placeholder keeps
            // the app alive enough for `TerminalGuard` to restore the terminal on exit.
            if let Ok(t) = rebuild(BASE_HEIGHT) {
                *terminal = t;
            }
            Err(err)
        }
    }
}

fn rebuild(height: u16) -> io::Result<Terminal<Backend>> {
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
}
