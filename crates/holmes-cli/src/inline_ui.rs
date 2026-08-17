//! Inline, Claude-Code-style TUI built on ratatui's Inline viewport.
//!
//! Unlike the legacy full-screen TUI (`tui.rs`, alternate screen + manual redraw), this
//! renders in the **main terminal buffer** with a two-layer model: completed output is
//! frozen into the native scrollback via `Terminal::insert_before` (immutable once
//! committed — so scroll / select / copy just work), and only a small live region at
//! the bottom is redrawn per frame. The live region is laid out
//! `[approval card][markdown tail][@completion][input box][status line]`; its height is
//! dynamic (`ui::viewport::set_live_height` rebuilds the `Terminal` to grow/shrink it
//! around a card or streaming tail). The terminal caret is real
//! (`frame.set_cursor_position`), so it advances as you type — including CJK (measured
//! with `unicode-width`).
//!
//! Rendering and input are split into `ui/` modules: `theme` (semantic colors/glyphs
//! with terminal-capability degradation), `wrap` (word-aware style-preserving wrapping
//! for scrollback commits), `input` (the ONE stdin reader thread both the idle and busy
//! loops drain, plus resize debounce), `keys` (`KeyOwner`/`EscStep` — key dispatch and
//! hint text derive from this single source), `viewport` (dynamic live-region height),
//! `permission` (Ask-mode approval card + `InlineApprover` runtime bridge), `blocks`
//! (structured tool blocks with folding + verb aggregation), `diff`/`highlight`
//! (syntect-highlighted diffs for file-writing tools), `markdown` (streaming markdown
//! with checkpoint freezing), `buffer` (segment input buffer with atomic paste chips),
//! `completion` (`@path` fuzzy file completion with ghost text).
//!
//! Built on ratatui 0.30 (whose `ratatui::crossterm` is 0.29, unified with reedline and
//! the legacy `tui.rs`).

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr;

use holmes_runtime::permissions::ApprovalHandler;
use holmes_runtime::runtime::TurnOutcome;
use holmes_runtime::yield_stream::{RuntimeSink, RuntimeYield, StreamEvent};
use holmes_runtime::SteeringQueue;
use holmes_tools::ToolRegistry;

use crate::chat::{
    create_chat_context, handle_slash_command, run_runtime_input_with_sink, ChatContext,
    ChatStartup, SlashResult,
};
use crate::ui::blocks::{
    aggregate_lines, default_display_mode, wave_spans, DisplayMode, ToolBlock, ToolStatus,
};
use crate::ui::buffer::InputBuffer;
use crate::ui::completion::{
    accept_replacement, at_query, candidate_lines, ghost_suffix, AtQuery, FileCompleter,
    GenerationFence, Request, Response,
};
use crate::ui::input::{drain_pending, InputEvent, InputThread, ResizeDebounce};
use crate::ui::keys::{esc_step, EscStep, KeyOwner};
use crate::ui::markdown::{assistant_block, MarkdownStream, MAX_TAIL_ROWS};
use crate::ui::permission::{
    card_lines, compact_line, verdict_line, ApprovalRequest, CardView, InlineApprover,
    PermissionCard,
};
use crate::ui::theme::{glyphs, init_theme, theme};
use crate::ui::viewport::{LiveRegion, BASE_HEIGHT};
use crate::ui::wrap::fit_lines;

pub(crate) type Backend = CrosstermBackend<io::Stdout>;

/// Spinner frames for the active terminal capability level (braille, or ASCII fallback).
fn spinner_frames() -> &'static [&'static str] {
    glyphs().spinner
}

/// CLI entry: build the session context (shared with the classic TUI) and run the inline UI.
pub async fn run(
    resume_id: Option<String>,
    continue_last: bool,
    model: Option<String>,
    mode_str: String,
) -> Result<()> {
    let Some(ChatStartup { ctx, is_resume }) =
        create_chat_context(resume_id, continue_last, model, mode_str, false).await?
    else {
        return Ok(());
    };
    // Try the inline UI; if the terminal can't host an inline viewport, fall back to the
    // classic full-screen TUI with the same context so the user always gets a working UI.
    match run_inline_tui(ctx, is_resume).await? {
        None => Ok(()),
        Some((ctx, is_resume)) => {
            eprintln!("Holmes: inline UI unavailable in this terminal; using the classic UI.");
            crate::tui::run_tui_with_context(ctx, is_resume).await
        }
    }
}

/// RAII terminal-state guard: enables raw mode + bracketed paste on creation and ALWAYS
/// restores (disable both + show cursor + newline) on drop — including early `?` returns
/// and panic unwind. Without this, a failure in `Terminal::with_options` (e.g. the inline
/// viewport's cursor query timing out under some terminals/tmux/SSH) or any panic would
/// leave the terminal in raw mode → unusable / "crashed". Bracketed paste is what lets
/// the input thread see a multi-line paste as ONE `Event::Paste` (→ paste chips) instead
/// of a keystroke spray; terminals without support simply never send the event.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let _ = execute!(io::stdout(), EnableBracketedPaste);
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), Show);
        println!();
    }
}

/// Restore the terminal from a panic hook (before the default hook prints the message),
/// so a panic never leaves the user with a broken terminal.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), Show);
        original(info);
    }));
}

/// Run the inline TUI. Returns `Ok(None)` when it ran; `Ok(Some((ctx, is_resume)))` when the
/// terminal can't host an inline viewport (cursor query failed) so the caller can fall back.
pub async fn run_inline_tui(
    ctx: ChatContext,
    is_resume: bool,
) -> Result<Option<(ChatContext, bool)>> {
    install_panic_hook();
    // Quantize the theme to this terminal's color level before anything renders.
    init_theme();
    let guard = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(io::stdout());
    // Some terminals (certain tmux/SSH setups) don't answer the cursor-position query the
    // inline viewport needs. Hand the context back for a classic-UI fallback rather than
    // failing — the guard restores raw mode either way.
    let mut terminal = match Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(BASE_HEIGHT),
        },
    ) {
        Ok(t) => t,
        Err(_) => {
            drop(guard);
            return Ok(Some((ctx, is_resume)));
        }
    };

    let mut app = App::new(ctx);
    app.emit(&mut terminal, welcome_lines(is_resume))?;
    // Replay prior conversation (resume/continue) so the user sees the history.
    let history = history_lines(&app.ctx.runtime_session.messages);
    if !history.is_empty() {
        app.emit(
            &mut terminal,
            vec![dim("── earlier conversation ──".to_string())],
        )?;
        app.emit(&mut terminal, history)?;
    }
    app.draw(&mut terminal)?;

    let result = app.event_loop(&mut terminal).await;

    // Clear the live viewport; the guard restores raw mode + cursor on drop.
    let _ = terminal.clear();
    result.map(|()| None)
}

struct App {
    ctx: ChatContext,
    /// The prompt text as a segment buffer (plain text + atomic paste chips).
    /// Caret offsets are byte positions in `buffer.display()` (see ui::buffer).
    buffer: InputBuffer,
    /// Byte offset of the caret within the buffer's display text (char boundary,
    /// never inside a paste chip).
    cursor: usize,
    busy: bool,
    status: String,
    spinner: usize,
    show_tool_output: bool,
    history: Vec<String>,
    hist_pos: Option<usize>,
    /// Selected entry in the slash-command autocomplete (active while input starts with '/').
    menu_sel: usize,
    /// Permission mode to restore when a plan is approved/left (set on entering plan mode).
    plan_prev_mode: Option<holmes_core::config::PermissionMode>,
    /// The one stdin reader for the whole UI (idle loop and busy turn both drain it).
    input_thread: InputThread,
    /// `@path` completion: the worker handle (spawned on the first `@`), the current
    /// list state, and the generation fence rejecting stale worker responses.
    completer: Option<FileCompleter>,
    completion: Option<CompletionState>,
    fence: GenerationFence,
    /// Ask-mode approval hook installed on each turn's runtime, plus the receiving end
    /// of its request channel (drained by the busy turn loop to drive the card).
    approver: Arc<InlineApprover>,
    approval_rx: tokio::sync::mpsc::UnboundedReceiver<ApprovalRequest>,
    /// Live-region height state (grows above the input box while a card is open).
    live: LiveRegion,
    exit: bool,
}

/// Live `@path` completion list state. `candidates` holds the last accepted worker
/// response (kept while a newer query is in flight so the list doesn't flicker empty
/// between keystrokes); `pending` is the generation we're waiting on.
struct CompletionState {
    query: AtQuery,
    candidates: Vec<String>,
    selected: usize,
    pending: u64,
}

impl App {
    fn new(ctx: ChatContext) -> Self {
        // Seed input history with the questions already asked in this session (including a
        // resumed one), so Up recalls "our last question" — not just this run's inputs.
        let history = session_questions(&ctx.runtime_session.messages);
        let (approver, approval_rx) = InlineApprover::channel();
        Self {
            ctx,
            buffer: InputBuffer::new(),
            cursor: 0,
            busy: false,
            status: default_status(),
            spinner: 0,
            show_tool_output: false,
            history,
            hist_pos: None,
            menu_sel: 0,
            plan_prev_mode: None,
            input_thread: InputThread::spawn(),
            completer: None,
            completion: None,
            fence: GenerationFence::default(),
            approver,
            approval_rx,
            live: LiveRegion::new(),
            exit: false,
        }
    }

    /// Which surface owns the keyboard right now (idle only — during a turn the busy
    /// loop dispatches explicitly, the card first). Single source of truth: ui::keys.
    fn key_owner(&self) -> KeyOwner {
        let menu_active = !self.busy && !self.command_matches().is_empty();
        crate::ui::keys::key_owner(false, menu_active, self.completion_visible())
    }

    /// The `@path` completion list is on screen (state exists AND has candidates —
    /// an empty result set shows nothing, so it must not swallow ↑↓/Tab/Esc).
    fn completion_visible(&self) -> bool {
        !self.busy
            && self
                .completion
                .as_ref()
                .map(|c| !c.candidates.is_empty())
                .unwrap_or(false)
    }

    /// A compact context line for the idle status row: model, token usage, active goal.
    fn idle_status(&self) -> String {
        let model = self
            .ctx
            .config
            .llm
            .providers
            .first()
            .map(|p| p.model.as_str())
            .unwrap_or("?");
        let tok = &self.ctx.runtime_session.tokens;
        let goal = self
            .ctx
            .runtime_state
            .active_goal
            .as_deref()
            .map(|g| {
                let short: String = g.chars().take(28).collect();
                format!(" · goal: {}", short)
            })
            .unwrap_or_default();
        // Plan mode indicator: while awaiting approval the agent is read-only.
        let plan = if self.plan_prev_mode.is_some() {
            " · ◇ PLAN (/approve)"
        } else {
            ""
        };
        format!(
            "{model} · {} in/{} out{goal}{plan} · /help · ↑ recall",
            tok.input, tok.output
        )
    }

    /// Slash-command matches for the current input (empty unless input starts with '/').
    fn command_matches(&self) -> Vec<(String, String)> {
        let display = self.buffer.display();
        let Some(rest) = display.strip_prefix('/') else {
            return Vec::new();
        };
        // Only while still typing the command word (no space yet).
        if rest.contains(' ') {
            return Vec::new();
        }
        let prefix = rest.to_lowercase();
        // `all_command_hints` yields `/name`; strip the slash so typing `/go` matches
        // `goal` and the menu shows `/goal` (not `//goal`).
        self.ctx
            .command_registry
            .all_command_hints()
            .into_iter()
            .filter_map(|(name, desc)| {
                let bare = name.trim_start_matches('/').to_string();
                bare.to_lowercase()
                    .starts_with(&prefix)
                    .then_some((bare, desc))
            })
            .collect()
    }

    async fn event_loop(&mut self, terminal: &mut Terminal<Backend>) -> Result<()> {
        let mut resize = ResizeDebounce::new();
        while !self.exit {
            // Idle is fully event-driven (tick_demand → None): block until a key or
            // paste, a completion-worker response, or the end of a pending
            // resize-debounce window. No periodic tick burns redraws here.
            let wait = earliest(tick_demand(false, false), resize.remaining(Instant::now()));
            let mut dirty = false;
            tokio::select! {
                biased;
                ev = self.input_thread.rx.recv() => {
                    match ev {
                        // Input thread died (stdin broke) — leave rather than spin.
                        None => break,
                        Some(first) => {
                            let batch = drain_pending(&mut self.input_thread.rx, first);
                            dirty = self.handle_idle_batch(batch, terminal, &mut resize).await?;
                        }
                    }
                }
                Some(resp) = completion_recv(&mut self.completer) => {
                    dirty = accept_response(&self.fence, &mut self.completion, resp);
                }
                _ = sleep_opt(wait) => {}
            }
            // Accept any further worker responses (generation-fenced); a no-op when
            // nothing arrived or the worker doesn't exist yet.
            if self.poll_completion() {
                dirty = true;
            }
            if self.exit {
                break;
            }
            // A resize burst redraws only once it has settled (16ms quiet window).
            if resize.settled(Instant::now()) {
                resize.clear();
                dirty = true;
            }
            if !dirty {
                continue;
            }
            // Size the live region to the completion list; a no-op when unchanged.
            let rows = self.completion_rows() as u16;
            self.live.set_extra_rows(terminal, rows);
            self.draw(terminal)?;
        }
        Ok(())
    }

    /// Handle one drained batch of input events (the caller redraws once afterwards).
    /// Returns whether anything visible changed — resizes only mark the debounce
    /// window and don't count. Stops early once exit was requested.
    async fn handle_idle_batch(
        &mut self,
        batch: Vec<InputEvent>,
        terminal: &mut Terminal<Backend>,
        resize: &mut ResizeDebounce,
    ) -> Result<bool> {
        let mut dirty = false;
        for ev in batch {
            match ev {
                InputEvent::Key(key) => {
                    self.handle_key(key, terminal).await?;
                    dirty = true;
                }
                InputEvent::Paste(text) => {
                    self.handle_paste(&text);
                    dirty = true;
                }
                InputEvent::Resize(_, _) => resize.mark(Instant::now()),
            }
            if self.exit {
                break;
            }
        }
        Ok(dirty)
    }

    /// A bracketed paste while idle: into the segment buffer (chip or flattened text),
    /// then re-evaluate `@` completion like any other edit.
    fn handle_paste(&mut self, text: &str) {
        self.buffer.insert_paste(&mut self.cursor, text);
        self.menu_sel = 0;
        self.update_completion();
    }

    /// Recompute the `@` completion context after an edit and (re)query the worker.
    /// Called on every input mutation; response acceptance happens in `poll_completion`.
    fn update_completion(&mut self) {
        let display = self.buffer.display();
        let Some(query) = at_query(&display, self.cursor) else {
            self.completion = None;
            return;
        };
        let generation = self.fence.next_generation();
        // Lazily spawn the worker on the first `@` (process-wide, once).
        if self.completer.is_none() {
            if let Ok(cwd) = std::env::current_dir() {
                self.completer = FileCompleter::spawn(cwd).ok();
            }
        }
        if let Some(completer) = &self.completer {
            completer.query(Request {
                generation,
                query: query.query.clone(),
                hidden: query.hidden,
                dirs_only: query.dirs_only,
            });
        }
        match &mut self.completion {
            // Same `@` token, query evolved: keep showing the previous candidates
            // until the fresh response lands (anti-flicker).
            Some(state) if state.query.at_start == query.at_start => {
                state.query = query;
                state.pending = generation;
                state.selected = state.selected.min(state.candidates.len().saturating_sub(1));
            }
            _ => {
                self.completion = Some(CompletionState {
                    query,
                    candidates: Vec::new(),
                    selected: 0,
                    pending: generation,
                });
            }
        }
    }

    /// Drain worker responses; the generation fence drops answers to stale queries so
    /// an out-of-order result can never flash outdated candidates. Returns true when a
    /// response was accepted (the caller then redraws).
    fn poll_completion(&mut self) -> bool {
        let mut accepted = false;
        if let Some(completer) = &mut self.completer {
            while let Ok(resp) = completer.rx.try_recv() {
                if accept_response(&self.fence, &mut self.completion, resp) {
                    accepted = true;
                }
            }
        }
        accepted
    }

    /// Rows the completion list needs above the input box (0 when closed/empty).
    fn completion_rows(&self) -> usize {
        if self.completion_visible() {
            self.completion
                .as_ref()
                .map(|c| c.candidates.len().min(8))
                .unwrap_or(0)
        } else {
            0
        }
    }

    /// Accept the selected candidate: replace the query text with the path. A directory
    /// candidate keeps completion open (drill-down into it); a file candidate appends a
    /// space and closes the list. The `@` prefix itself stays — `expand_file_mentions`
    /// needs it at submit time (see `accept_replacement` for the `@!` case).
    fn accept_completion(&mut self) {
        let Some(state) = &self.completion else {
            return;
        };
        let Some(candidate) = state.candidates.get(state.selected).cloned() else {
            return;
        };
        let is_dir = candidate.ends_with('/');
        let (replace_from, insert) = accept_replacement(&state.query, &candidate);
        self.buffer
            .replace_range(replace_from, self.cursor, &insert, &mut self.cursor);
        if is_dir {
            self.update_completion();
        } else {
            self.completion = None;
        }
    }

    /// Ghost text drawn muted after the caret: the selected completion candidate's
    /// remainder (e.g. `@src/ma` → `in.rs`), or the slash menu's selected command
    /// completion + args hint (`/go` → `al [condition|clear]`). Only with the caret at
    /// end of input — mid-line ghosts would be visual noise.
    fn ghost_text(&self) -> String {
        if self.busy || self.cursor != self.buffer.display_len() {
            return String::new();
        }
        if let Some(state) = &self.completion {
            if let Some(candidate) = state.candidates.get(state.selected) {
                if let Some(rest) = ghost_suffix(&state.query, candidate) {
                    return rest;
                }
            }
        }
        let matches = self.command_matches();
        if let Some((name, _)) = matches.get(self.menu_sel).or_else(|| matches.first()) {
            let display = self.buffer.display();
            let typed = &display[1..];
            if let Some(rest) = name.strip_prefix(typed) {
                let mut ghost = rest.to_string();
                // Args hint comes from the command registry (canonical name lookup).
                let hint = self
                    .ctx
                    .command_registry
                    .resolve(name)
                    .and_then(|c| self.ctx.command_registry.get(c))
                    .and_then(|def| def.args_hint);
                if let Some(hint) = hint {
                    ghost.push(' ');
                    ghost.push_str(hint);
                }
                return ghost;
            }
        }
        String::new()
    }

    async fn handle_key(&mut self, key: KeyEvent, terminal: &mut Terminal<Backend>) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.buffer.is_empty() {
                    self.exit = true;
                } else {
                    self.buffer.clear();
                    self.cursor = 0;
                    self.completion = None;
                }
            }
            KeyCode::Char('d') if ctrl => self.exit = true,
            KeyCode::Char('u') if ctrl => {
                self.buffer.kill_to_start(&mut self.cursor);
                self.update_completion();
            }
            KeyCode::Char('w') if ctrl => {
                self.buffer.delete_word_back(&mut self.cursor);
                self.update_completion();
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.buffer.display_len(),
            KeyCode::Char('p') if ctrl => self.history_prev(),
            KeyCode::Char('n') if ctrl => self.history_next(),
            KeyCode::Char(c) => {
                self.buffer.insert_char(&mut self.cursor, c);
                self.menu_sel = 0;
                self.update_completion();
            }
            KeyCode::Backspace => {
                self.buffer.backspace(&mut self.cursor);
                self.menu_sel = 0;
                self.update_completion();
            }
            KeyCode::Delete => {
                self.buffer.delete_forward(&mut self.cursor);
                self.update_completion();
            }
            KeyCode::Left => {
                self.buffer.move_left(&mut self.cursor);
                self.update_completion();
            }
            KeyCode::Right => {
                self.buffer.move_right(&mut self.cursor);
                self.update_completion();
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.update_completion();
            }
            KeyCode::End => {
                self.cursor = self.buffer.display_len();
                self.update_completion();
            }
            KeyCode::Tab => {
                if self.completion_visible() {
                    self.accept_completion();
                } else {
                    self.accept_command_match();
                }
            }
            KeyCode::Up => {
                if self.completion_visible() {
                    let state = self.completion.as_mut().expect("visible above");
                    state.selected = state.selected.saturating_sub(1);
                } else {
                    let matches = self.command_matches();
                    if matches.is_empty() {
                        self.history_prev();
                    } else {
                        self.menu_sel = self.menu_sel.saturating_sub(1);
                    }
                }
            }
            KeyCode::Down => {
                if self.completion_visible() {
                    let state = self.completion.as_mut().expect("visible above");
                    if state.selected + 1 < state.candidates.len() {
                        state.selected += 1;
                    }
                } else {
                    let matches = self.command_matches();
                    if matches.is_empty() {
                        self.history_next();
                    } else if self.menu_sel + 1 < matches.len() {
                        self.menu_sel += 1;
                    }
                }
            }
            KeyCode::Enter => self.submit(terminal).await?,
            KeyCode::Esc => {
                // Esc steps back one layer (ui::keys): an open completion list closes
                // (keeping the text), a slash menu falls through to clearing the input
                // (which removes the '/' prefix and therefore the menu).
                match esc_step(self.key_owner(), false) {
                    EscStep::DismissFileCompletion => self.completion = None,
                    EscStep::DismissMenu | EscStep::ClearInput => {
                        self.buffer.clear();
                        self.cursor = 0;
                        self.completion = None;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn submit(&mut self, terminal: &mut Terminal<Backend>) -> Result<()> {
        // Chips expand to their full pasted content for submission; history keeps the
        // expanded text too, so recall shows exactly what was sent (status quo).
        let input = self.buffer.expanded().trim().to_string();
        self.buffer.clear();
        self.cursor = 0;
        self.completion = None;
        if input.is_empty() {
            return Ok(());
        }
        self.history.push(input.clone());
        self.hist_pos = None;

        if let Some(cmd) = input.strip_prefix('!') {
            // `!cmd` runs a shell command locally (operator convenience, like Claude Code) —
            // its output goes to scrollback, not to the agent.
            self.run_shell(cmd.trim(), terminal)?;
        } else if input.starts_with('/') {
            self.handle_command(&input, terminal).await?;
        } else {
            self.run_turn(input, terminal).await?;
        }
        // Drain any type-ahead queued during the turn.
        while let Some(next) = self.ctx.queued_turns.pop_front() {
            self.run_turn(next, terminal).await?;
        }
        Ok(())
    }

    /// Run a local shell command (`!cmd`) and commit its output to scrollback. The operator's
    /// own machine, explicitly invoked — not routed through the agent or egress guards.
    fn run_shell(&self, cmd: &str, terminal: &mut Terminal<Backend>) -> Result<()> {
        if cmd.is_empty() {
            return self.note(terminal, "usage: !<shell command>".into());
        }
        self.emit(
            terminal,
            vec![Line::from(vec![
                Span::styled(
                    "$ ",
                    Style::default()
                        .fg(theme().warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(cmd.to_string()),
            ])],
        )?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let output = std::process::Command::new(shell)
            .arg("-c")
            .arg(cmd)
            .output();
        let mut lines: Vec<Line<'static>> = Vec::new();
        match output {
            Ok(out) => {
                for l in String::from_utf8_lossy(&out.stdout).lines() {
                    lines.push(Line::from(Span::raw(l.to_string())));
                }
                for l in String::from_utf8_lossy(&out.stderr).lines() {
                    lines.push(Line::from(Span::styled(
                        l.to_string(),
                        Style::default().fg(theme().failure),
                    )));
                }
                if !out.status.success() {
                    lines.push(dim(format!("  exit: {}", out.status)));
                }
            }
            Err(e) => lines.push(Line::from(Span::styled(
                format!("failed to run command: {e}"),
                Style::default().fg(theme().failure),
            ))),
        }
        if lines.is_empty() {
            lines.push(dim("  (no output)".into()));
        }
        self.emit(terminal, lines)
    }

    async fn run_turn(&mut self, input: String, terminal: &mut Terminal<Backend>) -> Result<()> {
        self.run_turn_labeled(input.clone(), input, terminal).await
    }

    /// Run a turn showing `display` in the transcript but sending `prompt` to the agent. For
    /// normal input the two are equal; commands like `/plan` show a concise label while
    /// sending a longer framing prompt.
    async fn run_turn_labeled(
        &mut self,
        display: String,
        prompt: String,
        terminal: &mut Terminal<Backend>,
    ) -> Result<()> {
        // Expand `@path` mentions into the message sent to the agent (file contents inlined),
        // while showing the original text to the user.
        let (expanded, loaded) = expand_file_mentions(&prompt);
        self.emit(terminal, user_lines(&display))?;
        if !loaded.is_empty() {
            self.note(terminal, format!("  ↪ attached: {}", loaded.join(", ")))?;
        }
        let input = expanded;
        self.busy = true;
        self.status = working_status();
        self.draw(terminal)?;

        self.ctx.cancel.store(false, Ordering::Relaxed);
        let cancel = self.ctx.cancel.clone();
        // Completed type-ahead lines go into the shared steering queue DURING the turn
        // (see `dispatch_busy_key`); the runtime drains them at iteration boundaries.
        let steering = self.ctx.steering.clone();
        let approver: Arc<dyn ApprovalHandler> = self.approver.clone();

        // Stream runtime yields over a channel and drive the turn future through `select!`
        // against input events, approval requests, and a pacing tick (see `tick_demand`:
        // fast while the wave/spinner animates, slow while an approval card parks them).
        // The tick redraws the live region so the spinner/wave animates even while the
        // LLM call blocks with no intermediate yields — otherwise the turn future is a
        // single `await` and the animation freezes for the whole call.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
        // Accumulates streamed assistant text (TextDelta) and renders it as markdown:
        // closed blocks freeze into scrollback, the open tail shows in the live region.
        let mut stream = MarkdownStream::new();
        // The open markdown tail currently drawn above the input box.
        let mut tail: Vec<Line<'static>> = Vec::new();
        // Throttle clock for tail refreshes (≥120ms, and only on newline boundaries).
        let mut last_md_refresh = Instant::now() - Duration::from_secs(1);
        // Tool lifecycle state for this turn (running tools, aggregation buffer, …).
        // Arc-cloned registry BEFORE the runtime future borrows `&mut self.ctx`, so the
        // busy loop can classify tools (read-only?) without touching the borrowed ctx.
        let mut feed = ToolFeed::new(self.show_tool_output, self.ctx.registry.clone());
        // Busy-loop state lives in locals, not on `self`: the turn future below holds
        // `&mut self.ctx`, so the loop may only touch DISJOINT fields (`self.spinner`,
        // `self.status`, `self.live`, `self.approval_rx`, `self.input_thread`) — never a
        // whole-`self` method. `partial` is the in-progress type-ahead line echoed in the
        // input box (raw mode doesn't echo keystrokes); completed lines are pushed into
        // the shared `steering` queue cloned above.
        let mut partial = String::new();
        let mut card: Option<PermissionCard> = None;
        let mut card_queue: VecDeque<ApprovalRequest> = VecDeque::new();
        // Resize events during the turn are debounced (16ms quiet window) like idle.
        let mut resize = ResizeDebounce::new();
        let result = {
            let mut sink = ChannelSink { tx };
            let fut =
                run_runtime_input_with_sink(&mut self.ctx, input, false, &mut sink, Some(approver));
            tokio::pin!(fut);
            loop {
                // Pacing for the next wake: wave/spinner animating → fast tier; card
                // open (animation parked) → slow tier; a pending resize burst pulls
                // the wake forward to its 16ms settle point.
                let wait = earliest(
                    tick_demand(true, card.is_none()),
                    resize.remaining(Instant::now()),
                );
                tokio::select! {
                    biased;
                    r = &mut fut => break r,
                    Some(event) = rx.recv() => {
                        push_stream(terminal, &mut stream, &mut feed, &mut tail, event.data);
                        // push_stream may have finalized the stream (clearing the tail)
                        // or left new deltas pending — keep the live-region height in
                        // sync with what's actually drawn. No-ops when unchanged.
                        sync_live_height(terminal, &mut self.live, card.as_ref(), tail.len() as u16);
                        refresh_markdown(
                            terminal, &mut stream, &mut self.live, &mut tail,
                            &mut last_md_refresh, card.is_some(),
                        );
                        self.spinner = (self.spinner + 1) % spinner_frames().len();
                        let line = busy_status_line(&feed.active, self.spinner, &self.status, card.is_some());
                        render_busy(terminal, line, &partial, card.as_ref(), &tail, &self.live)?;
                    }
                    Some(req) = self.approval_rx.recv() => {
                        // Ask mode: the runtime is blocked awaiting an answer — park the
                        // wave and open the approval card (queueing behind any active one).
                        card_queue.push_back(req);
                        if card.is_none() {
                            card = card_queue.pop_front().map(PermissionCard::new);
                        }
                        self.status = approval_waiting_status();
                        sync_live_height(terminal, &mut self.live, card.as_ref(), tail.len() as u16);
                        let line = busy_status_line(&feed.active, self.spinner, &self.status, true);
                        render_busy(terminal, line, &partial, card.as_ref(), &tail, &self.live)?;
                    }
                    Some(first) = self.input_thread.rx.recv() => {
                        // Drain everything already queued and render ONCE per batch:
                        // a keystroke/type-ahead burst then costs a single frame.
                        let batch = drain_pending(&mut self.input_thread.rx, first);
                        let mut dirty = false;
                        for ev in batch {
                            match ev {
                                InputEvent::Key(key) => {
                                    dirty = true;
                                    if card.is_some() {
                                        // The card owns every key until it resolves.
                                        let active = card.as_mut().expect("card checked above");
                                        if let Some(decision) = active.handle_key(key) {
                                            push_scrollback(terminal, vec![verdict_line(
                                                active,
                                                decision.allow,
                                                decision.always_scope.as_deref(),
                                            )]);
                                            // Rejection feedback becomes a steering message so
                                            // the agent sees WHY the call was denied.
                                            if let Some(feedback) = decision.feedback {
                                                steering
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner())
                                                    .push_back(format!(
                                                        "Feedback on the rejected {} call: {feedback}",
                                                        active.name
                                                    ));
                                            }
                                            card = card_queue.pop_front().map(PermissionCard::new);
                                            if card.is_none() {
                                                self.status = working_status();
                                            }
                                        }
                                    } else {
                                        dispatch_busy_key(key, &cancel, &mut partial, &steering);
                                    }
                                }
                                // Paste during a turn: type-ahead is a plain String, so the
                                // chip model doesn't apply — flatten newlines like a small
                                // paste and echo it (the card still owns all input).
                                InputEvent::Paste(text) if card.is_none() => {
                                    dirty = true;
                                    partial.push_str(&text.replace('\n', " "));
                                }
                                InputEvent::Resize(_, _) => resize.mark(Instant::now()),
                                _ => {}
                            }
                        }
                        // A pure resize burst waits for its debounce window (the tick
                        // branch draws once it settles); anything else renders now.
                        if resize.settled(Instant::now()) {
                            resize.clear();
                            dirty = true;
                        }
                        if dirty {
                            // Selection/scope/expansion changes alter the row count.
                            sync_live_height(terminal, &mut self.live, card.as_ref(), tail.len() as u16);
                            let line = busy_status_line(&feed.active, self.spinner, &self.status, card.is_some());
                            render_busy(terminal, line, &partial, card.as_ref(), &tail, &self.live)?;
                        }
                    }
                    _ = sleep_opt(wait) => {
                        if resize.settled(Instant::now()) {
                            resize.clear();
                        }
                        refresh_markdown(
                            terminal, &mut stream, &mut self.live, &mut tail,
                            &mut last_md_refresh, card.is_some(),
                        );
                        self.spinner = (self.spinner + 1) % spinner_frames().len();
                        let line = busy_status_line(&feed.active, self.spinner, &self.status, card.is_some());
                        render_busy(terminal, line, &partial, card.as_ref(), &tail, &self.live)?;
                    }
                }
            }
        };
        // Drain any yields buffered after the future returned (sink dropped → channel closed).
        while let Ok(event) = rx.try_recv() {
            push_stream(terminal, &mut stream, &mut feed, &mut tail, event.data);
        }
        // Commit any trailing streamed text that no non-delta yield finalized, then any
        // read-only tool blocks still sitting in the aggregation buffer.
        finalize_stream(terminal, &mut stream, &mut tail);
        feed.flush_pending(terminal);

        // Defensive: the turn future normally can't complete with a request still pending
        // (it awaits the answer), but never leave a oneshot dangling — fail closed.
        if let Some(mut active) = card.take() {
            active.deny_unanswered();
        }
        while let Some(queued) = card_queue.pop_front() {
            PermissionCard::new(queued).deny_unanswered();
        }
        while let Ok(req) = self.approval_rx.try_recv() {
            PermissionCard::new(req).deny_unanswered();
        }
        // Card and tail are gone: collapse the live region back to the base 4 rows.
        sync_live_height(terminal, &mut self.live, None, 0);

        // Steering leftovers (typed after the agent's last drain) were already moved
        // into `ctx.queued_turns` by `run_runtime_input_with_sink`; the submit loop
        // runs them as follow-up turns.

        self.busy = false;
        let mut status = match &result {
            Ok(TurnOutcome::Interrupted { .. }) => "Interrupted.".into(),
            Ok(TurnOutcome::NeedsUser { .. }) => "Awaiting your input.".into(),
            Ok(_) => default_status(),
            Err(error) => {
                self.emit(
                    terminal,
                    vec![Line::from(Span::styled(
                        format!("error: {error}"),
                        Style::default().fg(theme().failure),
                    ))],
                )?;
                "Turn failed.".into()
            }
        };
        // Surface still-running background subagents: their results will be injected
        // into the next turn (or can be pulled via get_task_output).
        let running = self.ctx.background_tasks.running_count();
        if running > 0 {
            status = format!("{status} · ⏳ {running} background task(s) running");
        }
        self.status = status;
        self.draw(terminal)?;
        Ok(())
    }

    async fn handle_command(&mut self, full: &str, terminal: &mut Terminal<Backend>) -> Result<()> {
        let body = full.trim_start_matches('/');
        let name = body.split(' ').next().unwrap_or("").to_lowercase();
        let args = body
            .split_once(' ')
            .map(|x| x.1)
            .unwrap_or("")
            .trim()
            .to_string();

        // UI-local commands (don't touch the shared handler / ChatContext).
        match name.as_str() {
            "tools" => {
                self.show_tool_output = !self.show_tool_output;
                return self.note(
                    terminal,
                    format!(
                        "tool output: {}",
                        if self.show_tool_output {
                            "expanded (all)"
                        } else {
                            "default folding"
                        }
                    ),
                );
            }
            "keys" | "shortcuts" => {
                return self.emit(terminal, help_lines());
            }
            "clear" | "cls" => {
                use ratatui::crossterm::cursor::MoveTo;
                use ratatui::crossterm::terminal::{Clear, ClearType};
                let _ = execute!(
                    io::stdout(),
                    Clear(ClearType::Purge),
                    Clear(ClearType::All),
                    MoveTo(0, 0)
                );
                let _ = terminal.clear();
                return self.emit(terminal, welcome_lines(false));
            }
            "plan" => return self.cmd_plan(&args, terminal).await,
            "approve" | "go" => return self.cmd_approve(terminal).await,
            "reject" | "revise" => return self.cmd_reject(&args, terminal).await,
            _ => {}
        }

        // Everything else delegates to the shared slash-command handler (all REPL commands),
        // which writes via `println!`. Capture its stdout so it doesn't corrupt the live
        // viewport, then re-emit it as clean scrollback via insert_before. (No raw-mode
        // toggle needed — the output never reaches the terminal directly.)
        let (result, captured) = {
            let redirect = gag::BufferRedirect::stdout().ok();
            let result = handle_slash_command(full, &mut self.ctx).await;
            let mut out = String::new();
            if let Some(mut r) = redirect {
                use std::io::Read;
                let _ = r.read_to_string(&mut out);
            }
            (result, out)
        };

        // Show the command's textual output (session lists, goal confirmations, …).
        let lines: Vec<Line<'static>> = captured
            .lines()
            .filter(|l| !l.contains('\u{1b}')) // drop escape-only lines (e.g. /clear)
            .map(|l| dim(l.to_string()))
            .collect();
        if !lines.is_empty() {
            self.emit(terminal, lines)?;
        }

        match result {
            SlashResult::Quit => self.exit = true,
            SlashResult::NewSession(assembled) => {
                crate::session_assembly::switch_to(&mut self.ctx, *assembled);
                self.history = session_questions(&self.ctx.runtime_session.messages);
                self.hist_pos = None;
                // Replay the (resumed) session's conversation into scrollback.
                let hist = history_lines(&self.ctx.runtime_session.messages);
                if !hist.is_empty() {
                    self.emit(terminal, vec![dim("── conversation ──".to_string())])?;
                    self.emit(terminal, hist)?;
                }
            }
            SlashResult::NotHandled(msg) => {
                self.note(terminal, format!("unknown command: {}", msg.trim()))?;
            }
            SlashResult::Handled => {}
        }

        // Setting a goal should immediately start working toward it — `/goal <objective>`
        // only records the condition, so kick off a turn on that objective (the agent then
        // iterates toward it and the goal is evaluated after the turn). `/goal` with no
        // args (query) or clear/stop does not start work.
        if name == "goal" && !args.is_empty() && !matches!(args.as_str(), "clear" | "stop" | "off")
        {
            if let Some(goal) = self.ctx.runtime_state.active_goal.clone() {
                self.note(terminal, "Starting work toward the goal…".to_string())?;
                self.run_turn(goal, terminal).await?;
            }
        }
        Ok(())
    }

    // ── plan-mode approval loop ──

    /// `/plan [objective]` — enter read-only plan mode. The agent may recon (read-only) and
    /// propose a step-by-step plan, but every mutating / exploit / egress-write tool is blocked
    /// by the `ReadOnly` permission mode until the operator `/approve`s. With an objective, a
    /// planning turn starts immediately.
    async fn cmd_plan(&mut self, objective: &str, terminal: &mut Terminal<Backend>) -> Result<()> {
        use holmes_core::config::PermissionMode;
        if self.plan_prev_mode.is_none() {
            self.plan_prev_mode = Some(self.ctx.config.permissions.mode.clone());
        }
        self.ctx.config.permissions.mode = PermissionMode::ReadOnly;
        self.note(
            terminal,
            "◇ Plan mode — read-only. The agent will investigate and propose a plan; nothing \
             mutating runs until you /approve (or /reject to revise)."
                .into(),
        )?;
        let objective = objective.trim();
        if objective.is_empty() {
            return Ok(());
        }
        let prompt = format!(
            "You are in PLAN MODE (read-only permission). Investigate read-only as needed and \
             produce a concrete, numbered, step-by-step plan to achieve this objective:\n\n{objective}\n\n\
             Record the plan with the write_todos tool. Do NOT perform any mutating, exploitative, \
             or state-changing actions yet — recon and planning only (any such tool call will be \
             blocked). When the plan is ready, stop and wait for the operator to approve it.",
        );
        self.run_turn_labeled(format!("[plan] {objective}"), prompt, terminal)
            .await
    }

    /// `/approve` — leave plan mode, restore the prior permission mode, and tell the agent to
    /// execute the plan it proposed.
    async fn cmd_approve(&mut self, terminal: &mut Terminal<Backend>) -> Result<()> {
        let Some(prev) = self.plan_prev_mode.take() else {
            return self.note(
                terminal,
                "Nothing to approve — not in plan mode. Use /plan <objective> first.".into(),
            );
        };
        self.ctx.config.permissions.mode = prev.clone();
        self.note(
            terminal,
            format!("✓ Plan approved — executing (permission mode: {prev})."),
        )?;
        self.run_turn_labeled(
            "[approved]".into(),
            "The plan is approved. Proceed to execute it now, step by step.".into(),
            terminal,
        )
        .await
    }

    /// `/reject [note]` — stay in read-only plan mode and ask the agent to revise the plan.
    async fn cmd_reject(&mut self, note: &str, terminal: &mut Terminal<Backend>) -> Result<()> {
        if self.plan_prev_mode.is_none() {
            return self.note(terminal, "Not in plan mode — nothing to reject.".into());
        }
        let note = note.trim();
        self.note(
            terminal,
            "✗ Plan rejected — still read-only. Revising…".into(),
        )?;
        let prompt = if note.is_empty() {
            "The operator rejected the plan. Revise it — reconsider the approach and produce an \
             improved read-only plan with write_todos, then wait for approval."
                .to_string()
        } else {
            format!(
                "The operator rejected the plan with this feedback:\n\n{note}\n\nRevise the plan \
                 accordingly (read-only), record it with write_todos, and wait for approval."
            )
        };
        self.run_turn_labeled(format!("[reject] {note}"), prompt, terminal)
            .await
    }

    // ── input editing ──
    // Char-level editing lives in `ui::buffer::InputBuffer` (segment model: text +
    // atomic paste chips); the caret is a byte offset into `buffer.display()`.
    /// Fill the input with the selected slash-command match (ready for args).
    fn accept_command_match(&mut self) {
        let matches = self.command_matches();
        if let Some((name, _)) = matches.get(self.menu_sel).or_else(|| matches.first()) {
            self.buffer.set_text(&format!("/{name} "));
            self.cursor = self.buffer.display_len();
            self.menu_sel = 0;
        }
    }
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.hist_pos {
            Some(0) => 0,
            Some(p) => p - 1,
            None => self.history.len() - 1,
        };
        self.hist_pos = Some(pos);
        self.buffer.set_text(&self.history[pos].clone());
        self.cursor = self.buffer.display_len();
        self.completion = None;
    }
    fn history_next(&mut self) {
        match self.hist_pos {
            Some(p) if p + 1 < self.history.len() => {
                self.hist_pos = Some(p + 1);
                self.buffer.set_text(&self.history[p + 1].clone());
                self.cursor = self.buffer.display_len();
            }
            _ => {
                self.hist_pos = None;
                self.buffer.clear();
                self.cursor = 0;
            }
        }
        self.completion = None;
    }

    // ── rendering ──

    /// Commit lines to native scrollback (above the live viewport).
    fn emit(&self, terminal: &mut Terminal<Backend>, lines: Vec<Line<'static>>) -> Result<()> {
        push_scrollback(terminal, lines);
        Ok(())
    }

    fn note(&self, terminal: &mut Terminal<Backend>, text: String) -> Result<()> {
        self.emit(
            terminal,
            vec![Line::from(Span::styled(
                text,
                Style::default().fg(theme().text_faint),
            ))],
        )
    }

    fn draw(&mut self, terminal: &mut Terminal<Backend>) -> Result<()> {
        if self.busy {
            self.spinner = (self.spinner + 1) % spinner_frames().len();
        }
        // Slash-command autocomplete row (shown in place of the status line while typing a
        // command). Suppressed while busy so a running turn shows the status line instead.
        let matches = if self.busy {
            Vec::new()
        } else {
            self.command_matches()
        };
        // While the `@` completion list is up, the status row carries its hint instead
        // of the slash menu / idle line (the list itself renders above the input box).
        let menu_line = if self.completion_visible() {
            Some(Line::from(Span::styled(
                "  @ files · ↑↓ select · Tab accept · Esc close · type to filter",
                Style::default().fg(theme().text_faint),
            )))
        } else {
            build_menu_line(&matches, self.menu_sel)
        };
        let completion = if self.completion_visible() {
            let state = self.completion.as_ref().expect("visible above");
            candidate_lines(&state.candidates, state.selected)
        } else {
            Vec::new()
        };
        let ghost = self.ghost_text();
        // Context line for the idle status row (model / tokens / goal).
        let idle = self.idle_status();
        // No tools are tracked outside the turn loop, so a busy draw here always shows
        // the fallback "Working…" line (the loop re-renders with tool titles on tick).
        let busy_line = self
            .busy
            .then(|| busy_status_line(&[], self.spinner, &self.status, false));
        render_viewport(
            terminal,
            &self.buffer.display(),
            &self.buffer.chip_ranges(),
            self.cursor,
            &ghost,
            self.busy,
            busy_line,
            menu_line,
            &idle,
            None,
            &[], // no markdown tail outside the turn loop (it's a turn-local live preview)
            &completion,
        )
    }
}

/// Build the slash-command autocomplete row from the current matches (None if empty).
fn build_menu_line(matches: &[(String, String)], menu_sel: usize) -> Option<Line<'static>> {
    if matches.is_empty() {
        return None;
    }
    let sel = menu_sel.min(matches.len() - 1);
    const SHOWN: usize = 8;
    // Window the list so the selected item stays visible (e.g. /goal when you arrow down
    // to it); "+N" shows how many more match.
    let start = if matches.len() <= SHOWN {
        0
    } else {
        sel.saturating_sub(SHOWN / 2).min(matches.len() - SHOWN)
    };
    let mut spans = vec![Span::styled("⌘", Style::default().fg(theme().menu_fg))];
    for (i, (name, _)) in matches.iter().enumerate().skip(start).take(SHOWN) {
        let style = if i == sel {
            Style::default()
                .fg(theme().menu_sel_fg)
                .bg(theme().menu_sel_bg)
        } else {
            Style::default().fg(theme().menu_fg)
        };
        spans.push(Span::styled(format!(" /{name} "), style));
    }
    let remaining = matches.len().saturating_sub(start + SHOWN);
    if remaining > 0 {
        spans.push(Span::styled(
            format!("+{remaining} "),
            Style::default().fg(theme().text_faint),
        ));
    }
    spans.push(Span::styled(
        " Tab pick · ↑↓ · type to filter",
        Style::default().fg(theme().text_faint),
    ));
    Some(Line::from(spans))
}

/// Busy-loop key dispatch while NO approval card is open: Esc/Ctrl+C interrupts the
/// turn (same semantics as the old interrupt watcher), other printable input is
/// type-ahead — completed lines queue as follow-up turns, the in-progress line is
/// echoed in the input box via `partial`.
fn dispatch_busy_key(
    key: KeyEvent,
    cancel: &AtomicBool,
    partial: &mut String,
    steering: &SteeringQueue,
) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
        cancel.store(true, Ordering::Relaxed);
        partial.clear();
        return;
    }
    match key.code {
        KeyCode::Enter => {
            let text = partial.trim().to_string();
            partial.clear();
            if !text.is_empty() {
                // Steering (grok-build interjection): the completed line goes straight
                // into the queue the runtime drains at the next iteration boundary, so
                // the agent sees it mid-turn instead of as a follow-up turn. Anything
                // it never drained is moved to `queued_turns` when the turn ends.
                steering
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(text);
            }
        }
        KeyCode::Char(c) => partial.push(c),
        KeyCode::Backspace => {
            partial.pop();
        }
        _ => {}
    }
}

/// Grow/shrink the live region to fit the current card + markdown tail (neither →
/// base 4 rows). The card sits ABOVE the tail. Compact-mode terminals
/// (`can_resize == false`) keep the base height; the card is then rendered inside the
/// input row and the tail simply stays unrefreshed (freeze-on-flush still applies).
fn sync_live_height(
    terminal: &mut Terminal<Backend>,
    live: &mut LiveRegion,
    card: Option<&PermissionCard>,
    tail_rows: u16,
) {
    let card_rows = match card {
        Some(card) if live.can_resize => {
            let width = terminal.size().map(|s| s.width as usize).unwrap_or(80);
            card_lines(card, width).len() as u16
        }
        _ => 0,
    };
    live.set_extra_rows(terminal, card_rows + tail_rows);
}

// ── loop pacing ──

/// Fast tier: a turn is running and something on screen animates (tool wave /
/// spinner, markdown-tail throttle). Matches the historical fixed 120ms tick.
const TICK_ANIMATING: Duration = Duration::from_millis(120);
/// Slow tier: a turn is running but nothing animates (approval card open — the
/// wave is parked). Keeps liveness without burning redraws.
const TICK_PAUSED: Duration = Duration::from_millis(500);

/// How soon the event loop must wake on its own, or `None` when it is purely
/// event-driven. Idle with nothing pending is `None` — zero ticks, the loop blocks
/// on the input/completion channels (both wake it; completion responses ride a
/// tokio channel for exactly this reason). Busy with animation gets the fast tier,
/// busy without (card open) the slow tier.
fn tick_demand(busy: bool, animating: bool) -> Option<Duration> {
    if !busy {
        return None;
    }
    Some(if animating {
        TICK_ANIMATING
    } else {
        TICK_PAUSED
    })
}

/// Combine two optional deadlines: the earliest wins; `None` means "never".
fn earliest(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

/// Sleep for `d`, or pend forever when `None` (a disabled `select!` branch).
async fn sleep_opt(d: Option<Duration>) {
    match d {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending::<()>().await,
    }
}

/// Receive the next completion-worker response; pends forever while no worker
/// exists (the common case until the first `@`), which disables the branch.
async fn completion_recv(completer: &mut Option<FileCompleter>) -> Option<Response> {
    match completer {
        Some(c) => c.rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Apply one worker response to the completion state, generation-fenced: answers
/// to stale queries are dropped so an out-of-order result can never flash outdated
/// candidates. Returns true when the response was accepted (caller redraws).
fn accept_response(
    fence: &GenerationFence,
    completion: &mut Option<CompletionState>,
    resp: Response,
) -> bool {
    if !fence.accept(resp.generation) {
        return false;
    }
    if let Some(state) = completion {
        if state.pending == resp.generation {
            state.candidates = resp.matches;
            state.selected = state.selected.min(state.candidates.len().saturating_sub(1));
            return true;
        }
    }
    false
}

fn working_status() -> String {
    format!(
        "Working…  Esc: {} · type to queue",
        EscStep::InterruptTurn.label()
    )
}

fn approval_waiting_status() -> String {
    format!(
        "{} waiting for approval · 1-3 choose · ↑↓ move · Esc: {}",
        glyphs().pause,
        EscStep::RejectCard.label()
    )
}

/// Build the busy status line: while tools run it's `wave + current tool title +
/// elapsed` (or `N tools running` for concurrent calls); while the LLM is thinking it
/// stays the classic spinner + hint; while an approval card is open it freezes on the
/// pause glyph (the wave stopping IS the "waiting on you" signal).
fn busy_status_line(
    active: &[(ToolBlock, Instant)],
    step: usize,
    fallback: &str,
    paused: bool,
) -> Line<'static> {
    let faint = Style::default().fg(theme().text_faint);
    if paused {
        return Line::from(vec![
            Span::styled(
                format!("{} ", glyphs().pause),
                Style::default().fg(theme().accent_busy),
            ),
            Span::styled(fallback.to_string(), faint),
        ]);
    }
    if active.is_empty() {
        return Line::from(vec![
            Span::styled(
                format!("{} ", spinner_frames()[step % spinner_frames().len()]),
                Style::default().fg(theme().accent_busy),
            ),
            Span::styled(fallback.to_string(), faint),
        ]);
    }
    let mut spans = wave_spans(step);
    spans.push(Span::raw(" ".to_string()));
    let label = if active.len() == 1 {
        active[0].0.title_text()
    } else {
        format!("{} tools running", active.len())
    };
    spans.push(Span::styled(label, Style::default().fg(theme().text)));
    // Elapsed of the longest-running call, whole seconds — cheap and stable to read.
    let elapsed = active
        .iter()
        .map(|(_, t)| t.elapsed())
        .max()
        .unwrap_or_default();
    spans.push(Span::styled(
        format!("  {}s · Esc: interrupt", elapsed.as_secs()),
        faint,
    ));
    Line::from(spans)
}

/// Per-turn tool-event state. Running tools are tracked for the live-region status
/// line (never committed to scrollback until finished — scrollback is immutable, so a
/// "Running…" line could never be updated in place). Finished read-only blocks that
/// fold to a single line are buffered and flushed as one verb-group aggregate line as
/// soon as anything non-aggregatable arrives (see `ui::blocks`).
struct ToolFeed {
    /// Currently executing tools with their start time (drives the status line).
    active: Vec<(ToolBlock, Instant)>,
    /// Finished, collapsed, successful read-only blocks awaiting aggregation.
    pending_reads: Vec<ToolBlock>,
    /// call_ids already rendered as `⛔ blocked` (blocked calls emit no ToolStarted,
    /// only PermissionDecision + ToolFinished — so the Finished must be swallowed).
    blocked: Vec<Option<String>>,
    /// `/tools` on = every finished block renders Expanded; folding/aggregation off.
    expand_all: bool,
    registry: Arc<ToolRegistry>,
}

impl ToolFeed {
    fn new(expand_all: bool, registry: Arc<ToolRegistry>) -> Self {
        ToolFeed {
            active: Vec::new(),
            pending_reads: Vec::new(),
            blocked: Vec::new(),
            expand_all,
            registry,
        }
    }

    fn on_started(&mut self, name: String, call_id: Option<String>, args: Option<String>) {
        self.active
            .push((ToolBlock::running(name, call_id, args), Instant::now()));
    }

    fn on_finished(
        &mut self,
        terminal: &mut Terminal<Backend>,
        name: String,
        call_id: Option<String>,
        success: bool,
        content: String,
    ) {
        // Pair with the running entry (by call_id, falling back to name) to recover the
        // args captured at ToolStarted — ToolFinished carries none, and the title line
        // (Read <path>, Edit <path> +x/-y, …) needs them.
        let started = self
            .active
            .iter()
            .position(|(b, _)| match (&b.call_id, &call_id) {
                (Some(a), Some(c)) => a == c,
                _ => b.name == name,
            })
            .map(|pos| self.active.remove(pos).0);
        // A denied call was already rendered as `⛔ blocked`; don't render it twice.
        if call_id.is_some() && self.blocked.contains(&call_id) {
            return;
        }
        // `ToolResult::blocked` reports the name "guard" regardless of the real tool.
        // With a paired ToolStarted we keep the started block's real name/args; without
        // one (operator denial in Ask mode emits no ToolStarted) there is no tool
        // identity to show, so render the blocked line directly instead of a bogus
        // `✗ guard …` error block.
        if started.is_none() && name == "guard" {
            let reason = content.trim_start_matches("[GUARD] ").to_string();
            self.on_denied(terminal, &name, call_id, &reason);
            return;
        }
        let read_only = self.registry.is_read_only(&name);
        let mut block =
            started.unwrap_or_else(|| ToolBlock::running(name.clone(), call_id.clone(), None));
        if name != "guard" {
            block.name = name;
        }
        block.status = if success {
            ToolStatus::Ok
        } else {
            ToolStatus::Err
        };
        block.output = content;
        block.display_mode = if self.expand_all {
            DisplayMode::Expanded
        } else {
            default_display_mode(&block.name, read_only, success)
        };
        // Aggregation candidates: quiet single-line read-only successes. Errors and
        // write/command tools flush the buffer first, then render on their own — an
        // error must never hide inside an aggregate line.
        let aggregatable = success && read_only == Some(true) && !self.expand_all;
        if aggregatable {
            self.pending_reads.push(block);
        } else {
            self.flush_pending(terminal);
            push_scrollback(terminal, block.render());
        }
    }

    /// A permission/guard denial: render immediately (the blocked call emits no
    /// ToolStarted, so there is nothing in `active` to wait for) and remember the
    /// call_id so the matching ToolFinished is skipped.
    fn on_denied(
        &mut self,
        terminal: &mut Terminal<Backend>,
        tool_name: &str,
        call_id: Option<String>,
        reason: &str,
    ) {
        self.flush_pending(terminal);
        self.blocked.push(call_id);
        push_scrollback(
            terminal,
            vec![Line::from(Span::styled(
                format!("  ⛔ {tool_name} blocked — {reason}"),
                Style::default().fg(theme().failure),
            ))],
        );
    }

    /// Flush the aggregation buffer: one block renders normally, several collapse into
    /// a single `Read 2 files · Searched 1 pattern` line.
    fn flush_pending(&mut self, terminal: &mut Terminal<Backend>) {
        if self.pending_reads.is_empty() {
            return;
        }
        let blocks = std::mem::take(&mut self.pending_reads);
        push_scrollback(terminal, aggregate_lines(&blocks));
    }
}

/// Redraw the live region during a running turn: the busy status line (wave + current
/// tool, or spinner + hint) + any type-ahead the user is entering (echoed in the input
/// box, since raw mode won't) + the approval card and/or the open markdown tail above
/// the box. A free function so it composes with the `&mut self.ctx` borrow the runtime
/// future holds (the turn loop passes disjoint fields only).
fn render_busy(
    terminal: &mut Terminal<Backend>,
    status_line: Line<'static>,
    partial: &str,
    card: Option<&PermissionCard>,
    tail: &[Line<'static>],
    live: &LiveRegion,
) -> Result<()> {
    let view = card.map(|c| {
        if live.can_resize {
            let width = terminal.size().map(|s| s.width as usize).unwrap_or(80);
            CardView::Full(card_lines(c, width))
        } else {
            CardView::Compact(compact_line(c))
        }
    });
    render_viewport(
        terminal,
        partial,
        &[],
        partial.len(),
        "",
        true,
        Some(status_line),
        None,
        "",
        view,
        tail,
        &[],
    )
}

/// Render the live viewport: `[card rows?] + [markdown tail rows?] + [completion
/// rows?] + input box + status/menu row`. A free function taking only the pieces it
/// needs — not `&mut App` — so it can be called from the turn loop (which holds a
/// mutable borrow of `App::ctx` via the runtime future) using disjoint field borrows.
/// `input` is display text (paste chips already rendered as `[Pasted: N lines]`
/// placeholders); `chips` marks which byte ranges are chips so they render accented;
/// `ghost` is muted suggestion text drawn after the caret.
#[allow(clippy::too_many_arguments)]
fn render_viewport(
    terminal: &mut Terminal<Backend>,
    input: &str,
    chips: &[(usize, usize)],
    cursor: usize,
    ghost: &str,
    busy: bool,
    busy_line: Option<Line<'static>>,
    menu_line: Option<Line<'static>>,
    idle: &str,
    card: Option<CardView>,
    tail: &[Line<'static>],
    completion: &[Line<'static>],
) -> Result<()> {
    let prompt = glyphs().prompt;
    let prompt_w = prompt.width() as u16;
    // Cursor's display column within the input text.
    let caret_cols = input[..cursor.min(input.len())].width() as u16;
    let input_owned = input.to_string();
    let chips_owned = chips.to_vec();
    let ghost_owned = ghost.to_string();
    let idle = idle.to_string();
    let tail_owned = tail.to_vec();
    let completion_owned = completion.to_vec();

    // DEC private mode 2026 (synchronized update): supporting terminals buffer the
    // whole frame and present it atomically on End, so a redraw can never tear.
    // Terminals without support silently ignore the sequences — safe to always send.
    let _ = execute!(io::stdout(), BeginSynchronizedUpdate);
    let drawn = terminal.draw(|frame| {
        let area = frame.area();
        // Full-height card rows sit above the input box; the compact fallback has no
        // rows of its own and is rendered INSIDE the input box below. The markdown
        // tail and the `@` completion list go between card and input (they never
        // co-occur: tail is busy-only, completion idle-only). Clamp all three to what
        // the viewport actually has (live.height may have been capped by MAX_HEIGHT).
        let (card_rows, compact_card) = match &card {
            Some(CardView::Full(lines)) => (lines.len() as u16, None),
            Some(CardView::Compact(line)) => (0, Some(line.clone())),
            None => (0, None),
        };
        // Input box (3: borders + text row) + status line (1) always survive.
        let extra = area.height.saturating_sub(4);
        let card_rows = card_rows.min(extra);
        let tail_rows = (tail_owned.len() as u16).min(extra.saturating_sub(card_rows));
        let completion_rows =
            (completion_owned.len() as u16).min(extra.saturating_sub(card_rows + tail_rows));
        if let Some(CardView::Full(lines)) = &card {
            frame.render_widget(
                Paragraph::new(lines.clone()),
                Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: card_rows,
                },
            );
        }
        if tail_rows > 0 {
            frame.render_widget(
                Paragraph::new(tail_owned.clone()),
                Rect {
                    x: area.x,
                    y: area.y + card_rows,
                    width: area.width,
                    height: tail_rows,
                },
            );
        }
        if completion_rows > 0 {
            frame.render_widget(
                Paragraph::new(completion_owned.clone()),
                Rect {
                    x: area.x,
                    y: area.y + card_rows + tail_rows,
                    width: area.width,
                    height: completion_rows,
                },
            );
        }
        let input_area = Rect {
            x: area.x,
            y: area.y + card_rows + tail_rows + completion_rows,
            width: area.width,
            height: area
                .height
                .saturating_sub(card_rows + tail_rows + completion_rows + 1),
        };
        let status_area = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(1),
            width: area.width,
            height: 1,
        };

        let border = Style::default().fg(if busy {
            theme().border_busy
        } else {
            theme().border
        });
        let block = Block::default().borders(Borders::ALL).border_style(border);
        let inner = block.inner(input_area);
        frame.render_widget(block, input_area);

        // Prompt (fixed) + input text in its own area with horizontal scroll so long
        // input keeps the caret visible instead of overflowing the box.
        frame.render_widget(
            Paragraph::new(Span::styled(prompt, Style::default().fg(theme().accent))),
            Rect {
                x: inner.x,
                y: inner.y,
                width: prompt_w,
                height: 1,
            },
        );
        let text_area = Rect {
            x: inner.x + prompt_w,
            y: inner.y,
            width: inner.width.saturating_sub(prompt_w),
            height: 1,
        };
        let avail = text_area.width;
        // Scroll so the caret stays within the visible text window.
        let scroll_x = caret_cols.saturating_sub(avail.saturating_sub(1));
        match compact_card {
            // Compact approval card: the input row carries the card prompt; the caret
            // parks at the row start (keys drive the card, not text editing).
            Some(line) => {
                frame.render_widget(Paragraph::new(line), text_area);
                frame.set_cursor_position((text_area.x, inner.y));
            }
            None => {
                // Style paste chips (accent) and append muted ghost text after the
                // caret; plain text stays unstyled.
                let mut spans: Vec<Span<'static>> = Vec::new();
                let mut pos = 0usize;
                for &(cs, ce) in &chips_owned {
                    let (cs, ce) = (cs.min(input_owned.len()), ce.min(input_owned.len()));
                    if cs > pos {
                        spans.push(Span::raw(input_owned[pos..cs].to_string()));
                    }
                    if ce > cs {
                        spans.push(Span::styled(
                            input_owned[cs..ce].to_string(),
                            Style::default()
                                .fg(theme().accent)
                                .add_modifier(Modifier::BOLD),
                        ));
                    }
                    pos = pos.max(ce);
                }
                if pos < input_owned.len() {
                    spans.push(Span::raw(input_owned[pos..].to_string()));
                }
                if !ghost_owned.is_empty() {
                    spans.push(Span::styled(
                        ghost_owned.clone(),
                        Style::default()
                            .fg(theme().text_faint)
                            .add_modifier(Modifier::DIM),
                    ));
                }
                frame.render_widget(
                    Paragraph::new(Line::from(spans)).scroll((0, scroll_x)),
                    text_area,
                );
                // Real caret, measured in display columns (CJK-correct).
                let cx = text_area.x + caret_cols.saturating_sub(scroll_x);
                frame.set_cursor_position((
                    cx.min(text_area.x + text_area.width.saturating_sub(1)),
                    inner.y,
                ));
            }
        }

        let status = if let Some(menu) = menu_line {
            menu
        } else if busy {
            // Both call sites always supply the line when busy; default is defensive.
            busy_line.unwrap_or_default()
        } else {
            Line::from(Span::styled(
                idle.clone(),
                Style::default()
                    .fg(theme().text_faint)
                    .add_modifier(Modifier::DIM),
            ))
        };
        frame.render_widget(Paragraph::new(status), status_area);
    });
    let _ = execute!(io::stdout(), EndSynchronizedUpdate);
    drawn?;
    Ok(())
}

/// Sink that forwards runtime yields over a channel to the turn loop, which owns the
/// terminal and commits them to scrollback between spinner ticks. Decoupling the sink from
/// the terminal lets the turn future be polled by `select!` (so the spinner keeps animating)
/// instead of blocking on a single `await` that holds a `&mut Terminal`.
struct ChannelSink {
    tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
}

impl RuntimeSink for ChannelSink {
    fn emit(&mut self, event: StreamEvent) {
        let _ = self.tx.send(event);
    }
}

/// Commit lines to native scrollback above the viewport. Wraps each line to the terminal
/// width ourselves (word-aware, display-width correct, styles preserved — see `ui::wrap`)
/// so `insert_before` gets the exact row count — ratatui's `Paragraph::line_count` is
/// private.
fn push_scrollback(terminal: &mut Terminal<Backend>, lines: Vec<Line<'static>>) {
    if lines.is_empty() {
        return;
    }
    let width = terminal.size().map(|s| s.width).unwrap_or(80).max(1) as usize;
    let fitted = fit_lines(lines, width);
    let height = fitted.len().max(1) as u16;
    let _ = terminal.insert_before(height, move |buf| {
        Paragraph::new(fitted).render(buf.area, buf);
    });
}

/// Feed one runtime yield through the markdown stream and the tool feed. Text deltas
/// accumulate into `MarkdownStream` (committed lazily via `refresh_markdown`, throttled
/// on the turn loop's tick); tool events route to `ToolFeed` (started → live region
/// only, finished → scrollback block, denied → `⛔` line); any non-delta yield first
/// FINALIZES the markdown stream — the LLM call is over, so the parser can safely close
/// open blocks at EOF and every remaining line commits — keeping scrollback order true
/// to event order. The full-text block that only duplicates what we streamed is dropped.
fn push_stream(
    terminal: &mut Terminal<Backend>,
    stream: &mut MarkdownStream,
    feed: &mut ToolFeed,
    tail: &mut Vec<Line<'static>>,
    data: RuntimeYield,
) {
    match data {
        RuntimeYield::TextDelta { content } => {
            // Text arriving after finished reads: fold the aggregation buffer first so
            // the aggregate line lands BEFORE the streamed text, not after.
            feed.flush_pending(terminal);
            stream.push_delta(&content);
        }
        RuntimeYield::ToolStarted {
            name,
            call_id,
            args,
        } => {
            // No scrollback: the running tool shows in the live-region status line
            // until its ToolFinished commits the finished block.
            finalize_stream(terminal, stream, tail);
            feed.on_started(name, call_id, args);
        }
        RuntimeYield::ToolFinished {
            name,
            call_id,
            success,
            content,
            ..
        } => {
            finalize_stream(terminal, stream, tail);
            feed.on_finished(terminal, name, call_id, success, content);
        }
        RuntimeYield::PermissionDecision {
            tool_name,
            call_id,
            allowed,
            reason,
        } => {
            if !allowed {
                finalize_stream(terminal, stream, tail);
                feed.on_denied(terminal, &tool_name, call_id, &reason);
            }
        }
        other => {
            let is_block = matches!(
                other,
                RuntimeYield::MessageToUser { .. } | RuntimeYield::FinalAnswer { .. }
            );
            finalize_stream(terminal, stream, tail);
            let suppress = stream.streamed() && is_block;
            if is_block {
                stream.clear_streamed();
            }
            if !suppress {
                feed.flush_pending(terminal);
                push_scrollback(terminal, yield_to_lines(other, term_width(terminal)));
            }
        }
    }
}

/// Final render of the streamed markdown: commit every not-yet-frozen line and clear
/// the live tail. Called before any non-delta yield touches scrollback.
fn finalize_stream(
    terminal: &mut Terminal<Backend>,
    stream: &mut MarkdownStream,
    tail: &mut Vec<Line<'static>>,
) {
    let rest = stream.finalize(term_width(terminal));
    if !rest.is_empty() {
        push_scrollback(terminal, rest);
    }
    tail.clear();
}

/// The terminal width renderers should target (matches `push_scrollback`).
fn term_width(terminal: &Terminal<Backend>) -> usize {
    terminal
        .size()
        .map(|s| s.width as usize)
        .unwrap_or(80)
        .max(1)
}

/// Throttled markdown refresh: freeze newly closed blocks into scrollback and redraw
/// the tail. Gates: a newline must have arrived since the last pass (rendering only
/// advances on line boundaries, so mid-word deltas never re-render), ≥120ms since the
/// last pass, and no approval card open (the card owns the live region; approval
/// happens after the LLM call ended, so the stream was already finalized → the tail
/// is empty then anyway).
#[allow(clippy::too_many_arguments)]
fn refresh_markdown(
    terminal: &mut Terminal<Backend>,
    stream: &mut MarkdownStream,
    live: &mut LiveRegion,
    tail: &mut Vec<Line<'static>>,
    last_refresh: &mut Instant,
    card_open: bool,
) {
    if card_open
        || !stream.has_pending_newline()
        || last_refresh.elapsed() < Duration::from_millis(120)
    {
        return;
    }
    *last_refresh = Instant::now();
    let width = term_width(terminal);
    // `terminal.size()` reports the inline VIEWPORT height, not the screen — take the
    // real terminal height for the half-screen tail cap.
    let screen_h = ratatui::crossterm::terminal::size()
        .map(|(_, h)| h as usize)
        .unwrap_or(24);
    let cap = MAX_TAIL_ROWS.min(screen_h / 2).max(1);
    let out = stream.refresh(width, cap);
    if !out.commit.is_empty() {
        push_scrollback(terminal, out.commit);
    }
    if out.tail != *tail {
        *tail = out.tail;
    }
    sync_live_height(terminal, live, None, tail.len() as u16);
}

/// Render a non-tool yield as scrollback lines. Tool lifecycle events never reach
/// here — `push_stream` routes them to `ToolFeed` (see above). Assistant text blocks
/// go through the full markdown renderer (with the `Holmes  ` header) — this is the
/// non-streaming path (`llm.stream=false`) and one-shot messages.
fn yield_to_lines(data: RuntimeYield, width: usize) -> Vec<Line<'static>> {
    match data {
        // Streaming deltas and tool lifecycle events are handled before reaching here.
        RuntimeYield::TextDelta { .. }
        | RuntimeYield::ToolStarted { .. }
        | RuntimeYield::ToolFinished { .. }
        | RuntimeYield::PermissionDecision { .. } => Vec::new(),
        RuntimeYield::MessageToUser { content }
        | RuntimeYield::PlanUpdate { content }
        | RuntimeYield::FinalAnswer { content, .. }
        | RuntimeYield::NeedsUserInput { prompt: content } => assistant_block(&content, width),
        RuntimeYield::EvidenceUpdate { content } => vec![Line::from(Span::styled(
            format!("  ▪ {content}"),
            Style::default().fg(theme().evidence),
        ))],
        // Confirmation that a mid-turn steering line was drained and the agent has now
        // seen it (muted — it is the operator's own text echoed back).
        RuntimeYield::SteeringInjected { content } => {
            vec![dim(format!("  ↪ steering: {content}"))]
        }
        // One-line notice for a background subagent completion; the full result was
        // injected into the conversation as a system-reminder.
        RuntimeYield::BackgroundTaskFinished {
            description,
            success,
            ..
        } => vec![dim(format!(
            "  ⚑ subagent \"{description}\" {}",
            if success { "completed" } else { "failed" }
        ))],
        RuntimeYield::CompactionBoundary {
            before_count,
            after_count,
            method,
            ..
        } => {
            vec![dim(format!(
                "  ↯ context compacted {before_count}→{after_count} ({method})"
            ))]
        }
        RuntimeYield::Error { message } => vec![Line::from(Span::styled(
            format!("  error: {message}"),
            Style::default().fg(theme().failure),
        ))],
    }
}

// ── helpers ──

fn dim(text: String) -> Line<'static> {
    Line::from(Span::styled(text, Style::default().fg(theme().text_faint)))
}

fn default_status() -> String {
    "/help commands · /tools toggle output · Ctrl+C clear · Ctrl+D quit".into()
}

fn welcome_lines(is_resume: bool) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            if is_resume {
                "▍ Resumed Holmes session"
            } else {
                "▍ Holmes"
            },
            Style::default()
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
        )),
        dim("  Type to chat. /help for commands.".into()),
    ]
}

fn help_lines() -> Vec<Line<'static>> {
    [
        "Commands:",
        "  /help          show this",
        "  /tools         toggle full tool output",
        "  /clear         clear the screen",
        "  /plan <goal>   read-only plan; then /approve to execute or /reject to revise",
        "  /quit          exit",
        "Input:  @path  attach a file (Tab completes; @! includes hidden)   ·   !cmd  run a shell command locally   ·   big pastes collapse into [Pasted: N lines] chips",
        "Keys: Enter send · ↑/↓ (or Ctrl+P/N) recall session questions · Ctrl+A/E line start/end · Ctrl+W delete word · Ctrl+U clear-to-start · Esc clear · Ctrl+C interrupt/clear · Ctrl+D quit · Tab pick command",
    ]
    .iter()
    .map(|l| dim(l.to_string()))
    .collect()
}

/// The user questions already asked in a session (for Up/Down input-history recall).
fn session_questions(messages: &[holmes_core::Message]) -> Vec<String> {
    use holmes_core::tool_types::Role;
    messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| m.content.as_deref())
        .filter(|c| !c.trim().is_empty())
        .map(|c| c.to_string())
        .collect()
}

/// Render a session's existing messages as scrollback so a resumed/continued
/// conversation shows its history (the classic TUI does this via rebuild_transcript).
fn history_lines(messages: &[holmes_core::Message]) -> Vec<Line<'static>> {
    use holmes_core::tool_types::Role;
    let mut out = Vec::new();
    for msg in messages {
        match msg.role {
            Role::User => {
                if let Some(c) = msg.content.as_deref() {
                    if !c.trim().is_empty() {
                        out.push(Line::from(vec![
                            Span::styled(
                                "You  ",
                                Style::default()
                                    .fg(theme().accent_user)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(c.to_string()),
                        ]));
                    }
                }
            }
            Role::Assistant => {
                if let Some(c) = msg.content.as_deref() {
                    if !c.trim().is_empty() {
                        out.push(Line::from(vec![
                            Span::styled(
                                "Holmes  ",
                                Style::default()
                                    .fg(theme().accent)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(c.to_string()),
                        ]));
                    }
                }
                if let Some(tcs) = &msg.tool_calls {
                    for tc in tcs {
                        out.push(dim(format!("  ⚙ {}", tc.function.name)));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Cap on how much of each `@`-mentioned file is inlined into the message.
const MAX_MENTION_BYTES: usize = 64 * 1024;

/// Expand `@path` file mentions in `input`: for each whitespace-delimited token that starts
/// with `@` and names a readable file, append its contents as a fenced block to the message
/// sent to the agent. Returns the expanded text and the list of files actually attached.
/// Tokens that don't resolve to a file are left untouched (so `@someone` stays literal).
fn expand_file_mentions(input: &str) -> (String, Vec<String>) {
    let mut blocks = String::new();
    let mut loaded = Vec::new();
    for token in input.split_whitespace() {
        let Some(path) = token.strip_prefix('@') else {
            continue;
        };
        if path.is_empty() || loaded.iter().any(|p| p == path) {
            continue;
        }
        let meta = std::fs::metadata(path);
        if !matches!(&meta, Ok(m) if m.is_file()) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let truncated = content.len() > MAX_MENTION_BYTES;
        // UTF-8 boundary-safe cut: String::truncate can panic mid-character.
        let content = holmes_core::truncate_str(&content, MAX_MENTION_BYTES);
        blocks.push_str(&format!(
            "\n\n--- contents of {path}{} ---\n{content}\n",
            if truncated { " (truncated)" } else { "" }
        ));
        loaded.push(path.to_string());
    }
    if blocks.is_empty() {
        (input.to_string(), loaded)
    } else {
        (format!("{input}{blocks}"), loaded)
    }
}

fn user_lines(input: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled(
            "You  ",
            Style::default()
                .fg(theme().accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(input.to_string()),
    ])]
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

    #[test]
    fn expand_file_mentions_inlines_existing_file_only() {
        let dir = std::env::temp_dir().join(format!("holmes_mention_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("note.txt");
        std::fs::write(&file, "secret-payload").unwrap();
        let input = format!("look at @{} and @nonexistent-xyz", file.display());
        let (expanded, loaded) = expand_file_mentions(&input);
        assert_eq!(loaded.len(), 1, "only the real file is attached");
        assert!(expanded.contains("secret-payload"), "file contents inlined");
        assert!(
            expanded.contains("@nonexistent-xyz"),
            "unresolved mention left literal"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn markdown_stream_prefixes_each_block_once() {
        let mut s = MarkdownStream::new();
        s.push_delta("hello\nworld");
        let first = s.finalize(80);
        assert!(
            plain(&first)[0].starts_with("Holmes"),
            "first line gets the header"
        );
        // Continuation lines are NOT re-prefixed or indented (markdown structures own
        // their indentation — see ui::markdown).
        assert!(!plain(&first)[1].starts_with("Holmes"));
        assert!(plain(&first)[1].contains("world"));
        // The next assistant block gets a fresh header.
        s.push_delta("second block");
        let second = s.finalize(80);
        assert!(plain(&second)[0].starts_with("Holmes"));
    }

    #[test]
    fn sse_delta_extractor_matches_runtime_yield() {
        // Sanity: a text delta produces displayable text, non-text yields do not.
        let out = yield_to_lines(
            RuntimeYield::TextDelta {
                content: "x".into(),
            },
            80,
        );
        assert!(
            out.is_empty(),
            "TextDelta is handled by push_stream, not yield_to_lines"
        );
        let out = yield_to_lines(
            RuntimeYield::ToolFinished {
                name: "read_file".into(),
                call_id: None,
                success: true,
                content: "x".into(),
                error: None,
                usage: None,
            },
            80,
        );
        assert!(
            out.is_empty(),
            "tool lifecycle events are handled by ToolFeed"
        );
    }

    #[test]
    fn non_streamed_message_block_renders_markdown_with_prefix() {
        // `llm.stream=false` path: a whole FinalAnswer arrives with no preceding
        // TextDelta — it must render through markdown (bold, header prefix).
        let out = yield_to_lines(
            RuntimeYield::FinalAnswer {
                content: "done **bold**".into(),
                usage: None,
            },
            80,
        );
        let text: String = plain(&out).join("");
        assert!(text.starts_with("Holmes"), "{text}");
        let bold = out[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn background_task_finished_renders_one_line_notice() {
        let out = yield_to_lines(
            RuntimeYield::BackgroundTaskFinished {
                task_id: "task-1".into(),
                description: "port scan".into(),
                success: true,
                summary: "done".into(),
            },
            80,
        );
        let text: String = plain(&out).join("");
        assert!(
            text.contains("⚑ subagent \"port scan\" completed"),
            "{text}"
        );

        let out = yield_to_lines(
            RuntimeYield::BackgroundTaskFinished {
                task_id: "task-2".into(),
                description: "fuzz params".into(),
                success: false,
                summary: "boom".into(),
            },
            80,
        );
        let text: String = plain(&out).join("");
        assert!(text.contains("⚑ subagent \"fuzz params\" failed"), "{text}");
    }

    #[test]
    fn expand_file_mentions_noop_without_mentions() {
        let (expanded, loaded) = expand_file_mentions("just a plain question");
        assert_eq!(expanded, "just a plain question");
        assert!(loaded.is_empty());
    }

    #[test]
    fn busy_status_line_shows_current_tool_title_and_elapsed() {
        let block = ToolBlock::running(
            "read_file".into(),
            Some("c1".into()),
            Some(r#"{"path":"src/main.rs"}"#.into()),
        );
        let active = vec![(block, Instant::now())];
        let text = plain(&[busy_status_line(&active, 0, "Working…", false)]).join("");
        assert!(
            text.contains("Read src/main.rs"),
            "tool title visible: {text}"
        );
        assert!(text.contains('s'), "elapsed seconds shown: {text}");
        assert!(text.contains("Esc"), "interrupt hint kept: {text}");
    }

    #[test]
    fn busy_status_line_counts_concurrent_tools() {
        let mk = |name: &str| (ToolBlock::running(name.into(), None, None), Instant::now());
        let active = vec![mk("grep"), mk("glob")];
        let text = plain(&[busy_status_line(&active, 0, "Working…", false)]).join("");
        assert!(text.contains("2 tools running"), "{text}");
    }

    #[test]
    fn busy_status_line_without_tools_falls_back_to_spinner_hint() {
        let text = plain(&[busy_status_line(&[], 1, "Working… hint", false)]).join("");
        assert!(text.contains("Working… hint"), "{text}");
    }

    #[test]
    fn busy_status_line_paused_freezes_on_pause_glyph() {
        let block = ToolBlock::running("grep".into(), None, None);
        let active = vec![(block, Instant::now())];
        let line = busy_status_line(&active, 5, "waiting for approval", true);
        let text = plain(std::slice::from_ref(&line)).join("");
        assert!(text.contains("waiting for approval"), "{text}");
        assert!(
            !text.contains("grep"),
            "tool title hidden while the card owns the status line: {text}"
        );
        // The wave must not animate: first span is the pause glyph, not a wave bar.
        assert_eq!(line.spans[0].content.trim(), glyphs().pause);
    }

    #[test]
    fn tick_demand_idle_is_event_driven() {
        assert_eq!(tick_demand(false, false), None, "idle: zero tick");
        assert_eq!(tick_demand(false, true), None, "idle never ticks");
    }

    #[test]
    fn tick_demand_busy_picks_fast_or_slow_tier() {
        assert_eq!(
            tick_demand(true, true),
            Some(TICK_ANIMATING),
            "animating turn: fast tier"
        );
        assert_eq!(
            tick_demand(true, false),
            Some(TICK_PAUSED),
            "card open (wave parked): slow tier"
        );
    }

    #[test]
    fn earliest_picks_the_sooner_deadline() {
        assert_eq!(earliest(None, None), None);
        let (a, b) = (Duration::from_millis(9), Duration::from_millis(3));
        assert_eq!(earliest(Some(a), None), Some(a));
        assert_eq!(earliest(None, Some(b)), Some(b));
        assert_eq!(earliest(Some(a), Some(b)), Some(b));
    }

    #[test]
    fn accept_response_applies_only_the_pending_generation() {
        let mut fence = GenerationFence::default();
        let g = fence.next_generation();
        let query = at_query("@s", 2).expect("@ query");
        let mut completion = Some(CompletionState {
            query,
            candidates: vec!["old".into()],
            selected: 0,
            pending: g,
        });
        let stale = Response {
            generation: g + 1,
            matches: vec!["stale".into()],
        };
        assert!(!accept_response(&fence, &mut completion, stale));
        assert_eq!(
            completion.as_ref().unwrap().candidates,
            vec!["old".to_string()],
            "stale generation dropped"
        );
        let fresh = Response {
            generation: g,
            matches: vec!["new".into()],
        };
        assert!(accept_response(&fence, &mut completion, fresh.clone()));
        assert_eq!(
            completion.as_ref().unwrap().candidates,
            vec!["new".to_string()]
        );
        // No open completion state: accepted-by-fence but nothing to update.
        assert!(!accept_response(&fence, &mut None, fresh));
    }
}
