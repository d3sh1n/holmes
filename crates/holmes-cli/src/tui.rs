use crate::chat::{
    create_chat_context, event_summary, event_type_label, folded_tool_output_summary,
    format_relative_time, load_session_runtime, parse_mode, refresh_guard_chain,
    run_runtime_input_with_sink, save_config, truncate_chars, ChatContext, ChatStartup,
};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TerminalEvent, KeyCode, KeyEvent,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use holmes_core::config::PermissionMode;
use holmes_core::event::StoredEvent;
use holmes_core::types::{SessionFilter, SessionSummary};
use holmes_guards::GuardChain;
use holmes_mind_palace::MindPalace;
use holmes_runtime::{RuntimeSink, RuntimeState, RuntimeYield, StreamEvent};
use holmes_session::selector::Selector;
use holmes_session::CreateSessionParams;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Stdout, Write};
use std::time::Duration;

const CHAT_MARGIN: u16 = 1;

pub async fn run_tui(
    resume_id: Option<String>,
    continue_last: bool,
    model: Option<String>,
    mode_str: String,
) -> anyhow::Result<()> {
    let Some(ChatStartup { ctx, is_resume }) =
        create_chat_context(resume_id, continue_last, model, mode_str, false).await?
    else {
        return Ok(());
    };

    let mut stdout = std::io::stdout();
    let _guard = TerminalGuard::enter(&mut stdout)?;
    let mut app = TuiApp::new(ctx, is_resume);
    app.refresh_sessions().await;
    app.refresh_events().await;
    app.rebuild_transcript_from_events().await;
    app.run(&mut stdout).await?;
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter(stdout: &mut Stdout) -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen, Hide, EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, DisableMouseCapture, Show, LeaveAlternateScreen);
    }
}

#[derive(Clone)]
enum EntryKind {
    User,
    Assistant,
    Tool,
    Permission,
    Evidence,
    System,
    Error,
}

#[derive(Clone)]
struct TuiEntry {
    kind: EntryKind,
    text: String,
}

impl TuiEntry {
    fn new(kind: EntryKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

#[derive(Clone)]
enum Overlay {
    None,
    Help,
    CommandPalette(SelectorOverlay),
    Sessions(SelectorOverlay),
    Tree(SelectorOverlay),
    Events(EventOverlay),
    Permissions(PermissionsOverlay),
    Guards(GuardsOverlay),
    Prompt(TextPrompt),
}

impl Default for Overlay {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Clone, Default)]
struct SelectorOverlay {
    query: String,
    selected: usize,
}

#[derive(Clone, Default)]
struct EventOverlay {
    query: String,
    selected: usize,
}

#[derive(Clone, Default)]
struct PermissionsOverlay {
    selected: usize,
}

#[derive(Clone, Default)]
struct GuardsOverlay {
    selected: usize,
}

#[derive(Clone)]
struct TextPrompt {
    title: String,
    label: String,
    value: String,
    action: PromptAction,
}

#[derive(Clone)]
enum PromptAction {
    RenameCurrent,
    PermissionAllow,
    PermissionDeny,
    ForkAt(u64),
}

struct TuiApp {
    ctx: ChatContext,
    entries: Vec<TuiEntry>,
    input: String,
    cursor: usize,
    scroll: usize,
    overlay: Overlay,
    sessions: Vec<SessionSummary>,
    events: Vec<StoredEvent>,
    collapsed_sessions: BTreeSet<String>,
    status: String,
    busy: bool,
    show_tool_output: bool,
    exit_requested: bool,
}

impl TuiApp {
    fn new(ctx: ChatContext, is_resume: bool) -> Self {
        let mut entries = Vec::new();
        entries.push(TuiEntry::new(
            EntryKind::System,
            if is_resume {
                "Resumed Holmes session."
            } else {
                "Started Holmes TUI session."
            },
        ));
        Self {
            ctx,
            entries,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            overlay: Overlay::None,
            sessions: Vec::new(),
            events: Vec::new(),
            collapsed_sessions: BTreeSet::new(),
            status: "F1 help  F2 tree  F3 events  F4 permissions  F5 guards  F6 sessions".into(),
            busy: false,
            show_tool_output: false,
            exit_requested: false,
        }
    }

    async fn run(&mut self, stdout: &mut Stdout) -> anyhow::Result<()> {
        self.render(stdout)?;
        while !self.exit_requested {
            if event::poll(Duration::from_millis(120))? {
                match event::read()? {
                    TerminalEvent::Key(key) => {
                        self.handle_key(key, stdout).await?;
                        self.render(stdout)?;
                    }
                    TerminalEvent::Mouse(ev) => {
                        self.handle_mouse(ev);
                        self.render(stdout)?;
                    }
                    _ => {}
                }
            } else if self.busy {
                self.render(stdout)?;
            }
        }

        Ok(())
    }

    /// Mouse wheel → scroll the chat viewport. Wheel up looks at older history
    /// (increase the offset from the bottom); wheel down returns toward the
    /// newest. Same direction as PgUp/PgDn, finer step.
    fn handle_mouse(&mut self, ev: MouseEvent) {
        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_add(3),
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
            _ => {}
        }
    }

    async fn handle_key(&mut self, key: KeyEvent, stdout: &mut Stdout) -> anyhow::Result<()> {
        if self.busy {
            if key.code == KeyCode::Esc {
                self.status = "Current turn is running; cancellation is not wired yet.".into();
            }
            return Ok(());
        }

        if !matches!(self.overlay, Overlay::None) {
            return self.handle_overlay_key(key, stdout).await;
        }

        match key {
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } if self.input.is_empty() => {
                self.exit_requested = true;
            }
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                if self.input.is_empty() {
                    self.exit_requested = true;
                } else {
                    self.input.clear();
                    self.cursor = 0;
                }
            }
            KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.new_session().await?,
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.fork_latest(None).await?,
            KeyEvent {
                code: KeyCode::Char('l'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.overlay = Overlay::CommandPalette(SelectorOverlay::default()),
            KeyEvent {
                code: KeyCode::Char('o'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.show_tool_output = !self.show_tool_output;
                self.status = format!(
                    "Tool output {}.",
                    if self.show_tool_output {
                        "expanded"
                    } else {
                        "folded"
                    }
                );
            }
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyEvent {
                code: KeyCode::F(1),
                ..
            } => self.overlay = Overlay::Help,
            KeyEvent {
                code: KeyCode::F(2),
                ..
            } => {
                self.refresh_sessions().await;
                self.overlay = Overlay::Tree(SelectorOverlay::default());
            }
            KeyEvent {
                code: KeyCode::F(3),
                ..
            } => {
                self.refresh_events().await;
                self.overlay = Overlay::Events(EventOverlay::default());
            }
            KeyEvent {
                code: KeyCode::F(4),
                ..
            } => self.overlay = Overlay::Permissions(PermissionsOverlay::default()),
            KeyEvent {
                code: KeyCode::F(5),
                ..
            } => self.overlay = Overlay::Guards(GuardsOverlay::default()),
            KeyEvent {
                code: KeyCode::F(6),
                ..
            } => {
                self.refresh_sessions().await;
                self.overlay = Overlay::Sessions(SelectorOverlay::default());
            }
            KeyEvent {
                code: KeyCode::PageUp,
                ..
            } => self.scroll = self.scroll.saturating_add(5),
            KeyEvent {
                code: KeyCode::PageDown,
                ..
            } => self.scroll = self.scroll.saturating_sub(5),
            KeyEvent {
                code: KeyCode::Home,
                ..
            } => self.cursor = 0,
            KeyEvent {
                code: KeyCode::End, ..
            } => self.cursor = self.input.len(),
            KeyEvent {
                code: KeyCode::Left,
                ..
            } => self.move_cursor_left(),
            KeyEvent {
                code: KeyCode::Right,
                ..
            } => self.move_cursor_right(),
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } => self.backspace(),
            KeyEvent {
                code: KeyCode::Delete,
                ..
            } => self.delete_at_cursor(),
            KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            } => {
                if modifiers.contains(KeyModifiers::ALT) {
                    self.insert_char('\n');
                } else {
                    self.submit(stdout).await?;
                }
            }
            KeyEvent {
                code: KeyCode::Char(ch),
                modifiers,
                ..
            } if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => self.insert_char(ch),
            _ => {}
        }
        Ok(())
    }

    async fn handle_overlay_key(
        &mut self,
        key: KeyEvent,
        stdout: &mut Stdout,
    ) -> anyhow::Result<()> {
        let overlay = std::mem::take(&mut self.overlay);
        self.overlay = match overlay {
            Overlay::None => Overlay::None,
            Overlay::Help => self.handle_help_key(key),
            Overlay::CommandPalette(mut state) => {
                self.handle_command_palette_key(&mut state, key).await?
            }
            Overlay::Sessions(mut state) => self.handle_sessions_key(&mut state, key).await?,
            Overlay::Tree(mut state) => self.handle_tree_key(&mut state, key).await?,
            Overlay::Events(mut state) => self.handle_events_key(&mut state, key).await?,
            Overlay::Permissions(mut state) => self.handle_permissions_key(&mut state, key).await?,
            Overlay::Guards(mut state) => self.handle_guards_key(&mut state, key).await?,
            Overlay::Prompt(mut prompt) => self.handle_prompt_key(&mut prompt, key, stdout).await?,
        };
        Ok(())
    }

    fn handle_help_key(&mut self, key: KeyEvent) -> Overlay {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) => Overlay::None,
            _ => Overlay::Help,
        }
    }

    async fn handle_command_palette_key(
        &mut self,
        state: &mut SelectorOverlay,
        key: KeyEvent,
    ) -> anyhow::Result<Overlay> {
        let rows = self.command_rows(&state.query);
        match key.code {
            KeyCode::Esc => Ok(Overlay::None),
            KeyCode::Up => {
                state.selected = state.selected.saturating_sub(1);
                Ok(Overlay::CommandPalette(state.clone()))
            }
            KeyCode::Down => {
                state.selected = (state.selected + 1).min(rows.len().saturating_sub(1));
                Ok(Overlay::CommandPalette(state.clone()))
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.selected = 0;
                Ok(Overlay::CommandPalette(state.clone()))
            }
            KeyCode::Enter => {
                if let Some((name, _)) = rows.get(state.selected) {
                    self.input = format!("{name} ");
                    self.cursor = self.input.len();
                    self.status = "Command inserted. Add arguments or press Enter.".into();
                }
                Ok(Overlay::None)
            }
            KeyCode::Char(ch) => {
                state.query.push(ch);
                state.selected = 0;
                Ok(Overlay::CommandPalette(state.clone()))
            }
            _ => Ok(Overlay::CommandPalette(state.clone())),
        }
    }

    async fn handle_sessions_key(
        &mut self,
        state: &mut SelectorOverlay,
        key: KeyEvent,
    ) -> anyhow::Result<Overlay> {
        let rows = self.session_rows(&state.query);
        match key.code {
            KeyCode::Esc => Ok(Overlay::None),
            KeyCode::Up => {
                state.selected = state.selected.saturating_sub(1);
                Ok(Overlay::Sessions(state.clone()))
            }
            KeyCode::Down => {
                state.selected = (state.selected + 1).min(rows.len().saturating_sub(1));
                Ok(Overlay::Sessions(state.clone()))
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.selected = 0;
                Ok(Overlay::Sessions(state.clone()))
            }
            KeyCode::Enter => {
                if let Some(row) = rows.get(state.selected) {
                    self.resume_session(&row.id).await?;
                }
                Ok(Overlay::None)
            }
            KeyCode::Char(ch) => {
                state.query.push(ch);
                state.selected = 0;
                Ok(Overlay::Sessions(state.clone()))
            }
            _ => Ok(Overlay::Sessions(state.clone())),
        }
    }

    async fn handle_tree_key(
        &mut self,
        state: &mut SelectorOverlay,
        key: KeyEvent,
    ) -> anyhow::Result<Overlay> {
        let rows = self.tree_rows(&state.query);
        match key.code {
            KeyCode::Esc => Ok(Overlay::None),
            KeyCode::Up => {
                state.selected = state.selected.saturating_sub(1);
                Ok(Overlay::Tree(state.clone()))
            }
            KeyCode::Down => {
                state.selected = (state.selected + 1).min(rows.len().saturating_sub(1));
                Ok(Overlay::Tree(state.clone()))
            }
            KeyCode::Left => {
                if let Some(row) = rows.get(state.selected) {
                    self.collapsed_sessions.insert(row.id.clone());
                }
                Ok(Overlay::Tree(state.clone()))
            }
            KeyCode::Right => {
                if let Some(row) = rows.get(state.selected) {
                    self.collapsed_sessions.remove(&row.id);
                }
                Ok(Overlay::Tree(state.clone()))
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.selected = 0;
                Ok(Overlay::Tree(state.clone()))
            }
            KeyCode::Enter => {
                if let Some(row) = rows.get(state.selected) {
                    self.resume_session(&row.id).await?;
                }
                Ok(Overlay::None)
            }
            KeyCode::Char('f') => {
                if let Some(row) = rows.get(state.selected) {
                    self.fork_session_latest(&row.id, None).await?;
                }
                Ok(Overlay::None)
            }
            KeyCode::Char(ch) => {
                state.query.push(ch);
                state.selected = 0;
                Ok(Overlay::Tree(state.clone()))
            }
            _ => Ok(Overlay::Tree(state.clone())),
        }
    }

    async fn handle_events_key(
        &mut self,
        state: &mut EventOverlay,
        key: KeyEvent,
    ) -> anyhow::Result<Overlay> {
        let rows = self.event_rows(&state.query);
        match key.code {
            KeyCode::Esc => Ok(Overlay::None),
            KeyCode::Up => {
                state.selected = state.selected.saturating_sub(1);
                Ok(Overlay::Events(state.clone()))
            }
            KeyCode::Down => {
                state.selected = (state.selected + 1).min(rows.len().saturating_sub(1));
                Ok(Overlay::Events(state.clone()))
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.selected = 0;
                Ok(Overlay::Events(state.clone()))
            }
            KeyCode::Enter | KeyCode::Char('f') => {
                if let Some(row) = rows.get(state.selected) {
                    self.overlay = Overlay::Prompt(TextPrompt {
                        title: "Fork from event".into(),
                        label: format!("Title for event {}:", row.event_index),
                        value: format!("branch at event {}", row.event_index),
                        action: PromptAction::ForkAt(row.event_index),
                    });
                    return Ok(std::mem::take(&mut self.overlay));
                }
                Ok(Overlay::Events(state.clone()))
            }
            KeyCode::Char(ch) => {
                state.query.push(ch);
                state.selected = 0;
                Ok(Overlay::Events(state.clone()))
            }
            _ => Ok(Overlay::Events(state.clone())),
        }
    }

    async fn handle_permissions_key(
        &mut self,
        state: &mut PermissionsOverlay,
        key: KeyEvent,
    ) -> anyhow::Result<Overlay> {
        let row_count = self.permission_row_count();
        match key.code {
            KeyCode::Esc => Ok(Overlay::None),
            KeyCode::Up => {
                state.selected = state.selected.saturating_sub(1);
                Ok(Overlay::Permissions(state.clone()))
            }
            KeyCode::Down => {
                state.selected = (state.selected + 1).min(row_count.saturating_sub(1));
                Ok(Overlay::Permissions(state.clone()))
            }
            KeyCode::Left => {
                if state.selected == 0 {
                    self.cycle_permission_mode(-1)?;
                }
                Ok(Overlay::Permissions(state.clone()))
            }
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => {
                if state.selected == 0 {
                    self.cycle_permission_mode(1)?;
                } else if state.selected == 1 {
                    self.ctx.config.permissions.auto_approve_read_only =
                        !self.ctx.config.permissions.auto_approve_read_only;
                    save_config(&self.ctx)?;
                    self.status = "Updated read-only auto approval.".into();
                }
                Ok(Overlay::Permissions(state.clone()))
            }
            KeyCode::Char('a') => Ok(Overlay::Prompt(TextPrompt {
                title: "Allow tool pattern".into(),
                label: "Pattern:".into(),
                value: String::new(),
                action: PromptAction::PermissionAllow,
            })),
            KeyCode::Char('d') => Ok(Overlay::Prompt(TextPrompt {
                title: "Deny tool pattern".into(),
                label: "Pattern:".into(),
                value: String::new(),
                action: PromptAction::PermissionDeny,
            })),
            KeyCode::Char('x') | KeyCode::Delete => {
                self.remove_selected_permission_pattern(state.selected)?;
                state.selected = state
                    .selected
                    .min(self.permission_row_count().saturating_sub(1));
                Ok(Overlay::Permissions(state.clone()))
            }
            KeyCode::Char('r') => {
                self.ctx.config.permissions.mode = PermissionMode::Default;
                self.ctx.config.permissions.allowed_tools.clear();
                self.ctx.config.permissions.disallowed_tools.clear();
                self.ctx.config.permissions.auto_approve_read_only = true;
                save_config(&self.ctx)?;
                self.status = "Permissions reset to default.".into();
                Ok(Overlay::Permissions(state.clone()))
            }
            _ => Ok(Overlay::Permissions(state.clone())),
        }
    }

    async fn handle_guards_key(
        &mut self,
        state: &mut GuardsOverlay,
        key: KeyEvent,
    ) -> anyhow::Result<Overlay> {
        let row_count = guard_rows().len() + 1;
        match key.code {
            KeyCode::Esc => Ok(Overlay::None),
            KeyCode::Up => {
                state.selected = state.selected.saturating_sub(1);
                Ok(Overlay::Guards(state.clone()))
            }
            KeyCode::Down => {
                state.selected = (state.selected + 1).min(row_count.saturating_sub(1));
                Ok(Overlay::Guards(state.clone()))
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.toggle_selected_guard(state.selected)?;
                Ok(Overlay::Guards(state.clone()))
            }
            KeyCode::Char('+') | KeyCode::Right => {
                if state.selected == row_count - 1 {
                    self.ctx.config.guards.repetition_window += 1;
                    refresh_guard_chain(&mut self.ctx);
                    save_config(&self.ctx)?;
                    self.status = "Repetition window increased.".into();
                }
                Ok(Overlay::Guards(state.clone()))
            }
            KeyCode::Char('-') | KeyCode::Left => {
                if state.selected == row_count - 1 {
                    self.ctx.config.guards.repetition_window = self
                        .ctx
                        .config
                        .guards
                        .repetition_window
                        .saturating_sub(1)
                        .max(1);
                    refresh_guard_chain(&mut self.ctx);
                    save_config(&self.ctx)?;
                    self.status = "Repetition window decreased.".into();
                }
                Ok(Overlay::Guards(state.clone()))
            }
            KeyCode::Char('a') => {
                let all_on = guard_rows().iter().all(|row| self.guard_enabled(row.key));
                for row in guard_rows() {
                    self.set_guard(row.key, !all_on);
                }
                refresh_guard_chain(&mut self.ctx);
                save_config(&self.ctx)?;
                self.status = format!(
                    "All guards {}.",
                    if all_on { "disabled" } else { "enabled" }
                );
                Ok(Overlay::Guards(state.clone()))
            }
            _ => Ok(Overlay::Guards(state.clone())),
        }
    }

    async fn handle_prompt_key(
        &mut self,
        prompt: &mut TextPrompt,
        key: KeyEvent,
        _stdout: &mut Stdout,
    ) -> anyhow::Result<Overlay> {
        match key.code {
            KeyCode::Esc => Ok(Overlay::None),
            KeyCode::Backspace => {
                prompt.value.pop();
                Ok(Overlay::Prompt(prompt.clone()))
            }
            KeyCode::Enter => {
                let value = prompt.value.trim().to_string();
                match prompt.action.clone() {
                    PromptAction::RenameCurrent => {
                        if !value.is_empty() {
                            self.ctx
                                .session_db
                                .set_title(&self.ctx.session_id, &value)
                                .await?;
                            self.status = format!("Renamed session to {value}.");
                            self.refresh_sessions().await;
                        }
                    }
                    PromptAction::PermissionAllow => {
                        if !value.is_empty()
                            && !self
                                .ctx
                                .config
                                .permissions
                                .allowed_tools
                                .iter()
                                .any(|p| p == &value)
                        {
                            self.ctx
                                .config
                                .permissions
                                .allowed_tools
                                .push(value.clone());
                            save_config(&self.ctx)?;
                            self.status = format!("Allowed pattern: {value}");
                        }
                    }
                    PromptAction::PermissionDeny => {
                        if !value.is_empty()
                            && !self
                                .ctx
                                .config
                                .permissions
                                .disallowed_tools
                                .iter()
                                .any(|p| p == &value)
                        {
                            self.ctx
                                .config
                                .permissions
                                .disallowed_tools
                                .push(value.clone());
                            save_config(&self.ctx)?;
                            self.status = format!("Denied pattern: {value}");
                        }
                    }
                    PromptAction::ForkAt(index) => {
                        let title = if value.is_empty() {
                            format!("branch at event {index}")
                        } else {
                            value
                        };
                        self.fork_at(index, Some(title)).await?;
                    }
                }
                Ok(Overlay::None)
            }
            KeyCode::Char(ch) => {
                prompt.value.push(ch);
                Ok(Overlay::Prompt(prompt.clone()))
            }
            _ => Ok(Overlay::Prompt(prompt.clone())),
        }
    }

    async fn submit(&mut self, stdout: &mut Stdout) -> anyhow::Result<()> {
        let input = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        if input.is_empty() {
            return Ok(());
        }

        if input.starts_with('/') {
            self.handle_tui_command(&input).await?;
            return Ok(());
        }

        self.run_turn(input, stdout).await?;
        self.drain_queued_turns(stdout).await
    }

    async fn handle_tui_command(&mut self, input: &str) -> anyhow::Result<()> {
        let mut parts = input.trim_start_matches('/').splitn(2, ' ');
        let cmd = parts.next().unwrap_or_default().to_ascii_lowercase();
        let args = parts.next().unwrap_or_default().trim();
        let canonical = self.ctx.command_registry.resolve(&cmd).unwrap_or(&cmd);

        match canonical {
            "quit" | "exit" | "q" => self.exit_requested = true,
            "help" => self.overlay = Overlay::Help,
            "tree" => {
                self.refresh_sessions().await;
                if args.starts_with("events") {
                    self.refresh_events().await;
                    self.overlay = Overlay::Events(EventOverlay::default());
                } else if let Some(rest) = args.strip_prefix("fork ") {
                    let mut bits = rest.splitn(2, ' ');
                    if let Some(raw) = bits.next() {
                        if let Ok(index) = raw.parse::<u64>() {
                            self.fork_at(index, bits.next().map(str::to_string)).await?;
                        }
                    }
                } else {
                    self.overlay = Overlay::Tree(SelectorOverlay::default());
                }
            }
            "sessions" | "history" | "resume" => {
                self.refresh_sessions().await;
                if canonical == "resume" && !args.is_empty() {
                    if let Some(id) = self.resolve_session_arg(args) {
                        self.resume_session(&id).await?;
                    } else {
                        self.status = format!("Session not found: {args}");
                    }
                } else {
                    self.overlay = Overlay::Sessions(SelectorOverlay::default());
                }
            }
            "permissions" | "permission" | "perm" => {
                self.overlay = Overlay::Permissions(PermissionsOverlay::default())
            }
            "guards" | "guard" => self.overlay = Overlay::Guards(GuardsOverlay::default()),
            "new" | "reset" | "clear" => self.new_session().await?,
            "branch" | "fork" => {
                let title = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                self.fork_latest(title).await?;
            }
            "rename" | "title" => {
                if args.is_empty() {
                    self.overlay = Overlay::Prompt(TextPrompt {
                        title: "Rename session".into(),
                        label: "Title:".into(),
                        value: String::new(),
                        action: PromptAction::RenameCurrent,
                    });
                } else {
                    self.ctx
                        .session_db
                        .set_title(&self.ctx.session_id, args)
                        .await?;
                    self.status = format!("Renamed session to {args}.");
                    self.refresh_sessions().await;
                }
            }
            "mode" => {
                if args.is_empty() {
                    self.status = format!("Current mode: {:?}", self.ctx.runtime_session.mode);
                } else {
                    let mode = parse_mode(args);
                    self.ctx.runtime_session.mode = mode.clone();
                    self.ctx.runtime_state.session_mode = mode.clone();
                    self.status = format!("Mode switched to {:?}.", mode);
                }
            }
            "status" | "session" => self.push_session_status().await,
            "dashboard" => self.push_dashboard(),
            "tools" => self.push_tools(args),
            "provider" | "model" | "config" | "usage" | "workflows" => {
                self.push_basic_info(canonical).await
            }
            _ => {
                self.entries.push(TuiEntry::new(
                    EntryKind::System,
                    format!("Unknown TUI command: /{cmd}. Press Ctrl+L for commands."),
                ));
            }
        }
        Ok(())
    }

    async fn run_turn(&mut self, input: String, stdout: &mut Stdout) -> anyhow::Result<()> {
        self.entries
            .push(TuiEntry::new(EntryKind::User, input.clone()));
        self.busy = true;
        self.status = "Holmes is working... Esc notes cancellation status.".into();
        self.render(stdout)?;

        let footer = self.footer_line();
        let mut sink = TuiRuntimeSink {
            entries: &mut self.entries,
            stdout,
            show_tool_output: self.show_tool_output,
            footer,
        };
        let result = run_runtime_input_with_sink(&mut self.ctx, input, false, &mut sink).await;
        drop(sink);
        self.busy = false;
        match result {
            Ok(_) => {
                self.status = "Turn complete.".into();
                self.refresh_events().await;
                self.refresh_sessions().await;
            }
            Err(error) => {
                self.entries
                    .push(TuiEntry::new(EntryKind::Error, format!("Error: {error}")));
                self.status = "Turn failed.".into();
            }
        }
        Ok(())
    }

    async fn drain_queued_turns(&mut self, stdout: &mut Stdout) -> anyhow::Result<()> {
        let mut queued = VecDeque::new();
        std::mem::swap(&mut self.ctx.queued_turns, &mut queued);
        while let Some(input) = queued.pop_front() {
            self.entries.push(TuiEntry::new(
                EntryKind::System,
                format!("Queued turn: {input}"),
            ));
            self.run_turn(input, stdout).await?;
        }
        Ok(())
    }

    async fn new_session(&mut self) -> anyhow::Result<()> {
        let session = self
            .ctx
            .session_db
            .create_session(CreateSessionParams {
                id: None,
                title: None,
                mode: Some(self.ctx.runtime_session.mode.clone()),
                model: None,
                system_prompt: Some(self.ctx.system_prompt.clone()),
                parent_session_id: None,
                fork_point: None,
                source: Some("tui".into()),
                tags: vec![],
            })
            .await?;
        let new_id = session.id.clone();
        self.ctx.session_id = new_id.clone();
        self.ctx.runtime_session =
            holmes_core::session::RuntimeSession::new(new_id.clone(), session.mode)
                .with_system_prompt(&self.ctx.system_prompt);
        self.ctx.mind_palace =
            MindPalace::new(self.ctx.session_db.clone(), self.ctx.memory_store.clone());
        self.ctx.runtime_state = RuntimeState::new(self.ctx.runtime_session.mode.clone());
        self.ctx.runtime_guards = GuardChain::from_config(&self.ctx.config.guards);
        self.ctx.queued_turns.clear();
        self.ctx.steering_notes.clear();
        self.rebuild_selector();
        self.entries.clear();
        self.entries.push(TuiEntry::new(
            EntryKind::System,
            format!("Started new session {}.", short_id(&new_id)),
        ));
        self.refresh_sessions().await;
        self.refresh_events().await;
        Ok(())
    }

    async fn resume_session(&mut self, session_id: &str) -> anyhow::Result<()> {
        let Some(session_record) = self.ctx.session_db.get_session(session_id).await? else {
            self.status = format!("Session not found: {session_id}");
            return Ok(());
        };
        let (runtime_session, mind_palace) =
            load_session_runtime(&self.ctx, &session_record.id, session_record.mode.clone())
                .await?;
        self.ctx.session_id = session_record.id.clone();
        self.ctx.runtime_session = runtime_session;
        self.ctx.mind_palace = mind_palace;
        self.ctx.runtime_state = RuntimeState::new(self.ctx.runtime_session.mode.clone());
        self.ctx.runtime_state.active_goal = session_record.goal_condition;
        self.ctx.runtime_guards = GuardChain::from_config(&self.ctx.config.guards);
        self.ctx.queued_turns.clear();
        self.ctx.steering_notes.clear();
        self.rebuild_selector();
        self.refresh_events().await;
        self.rebuild_transcript_from_events().await;
        self.refresh_sessions().await;
        self.status = format!("Resumed session {}.", short_id(&self.ctx.session_id));
        Ok(())
    }

    async fn fork_latest(&mut self, title: Option<String>) -> anyhow::Result<()> {
        let events = self.ctx.session_db.get_events(&self.ctx.session_id).await?;
        let fork_point = events
            .last()
            .map(|event| event.event_index)
            .unwrap_or_default();
        self.fork_at(fork_point, title).await
    }

    async fn fork_session_latest(
        &mut self,
        session_id: &str,
        title: Option<String>,
    ) -> anyhow::Result<()> {
        let events = self.ctx.session_db.get_events(session_id).await?;
        let fork_point = events
            .last()
            .map(|event| event.event_index)
            .unwrap_or_default();
        let title = title.unwrap_or_else(|| "branch".into());
        let new_session = self
            .ctx
            .session_db
            .fork_session(session_id, fork_point, &title)
            .await?;
        self.resume_session(&new_session.id).await
    }

    async fn fork_at(&mut self, event_index: u64, title: Option<String>) -> anyhow::Result<()> {
        let title = title.unwrap_or_else(|| format!("branch at event {event_index}"));
        let new_session = self
            .ctx
            .session_db
            .fork_session(&self.ctx.session_id, event_index, &title)
            .await?;
        self.resume_session(&new_session.id).await?;
        self.status = format!(
            "Forked at event {event_index} into {}.",
            short_id(&new_session.id)
        );
        Ok(())
    }

    fn rebuild_selector(&mut self) {
        let mut selector = Selector::new();
        for wf in crate::workflows::create_builtin_workflows(
            self.ctx.llm.clone(),
            self.ctx.registry.clone(),
            self.ctx.guards.clone(),
        ) {
            selector.register(wf);
        }
        self.ctx.selector = selector;
    }

    async fn refresh_sessions(&mut self) {
        self.sessions = self
            .ctx
            .session_db
            .list_sessions(&SessionFilter {
                include_children: true,
                limit: Some(300),
                ..Default::default()
            })
            .await
            .unwrap_or_default();
    }

    async fn refresh_events(&mut self) {
        self.events = self
            .ctx
            .session_db
            .get_events(&self.ctx.session_id)
            .await
            .unwrap_or_default();
    }

    async fn rebuild_transcript_from_events(&mut self) {
        self.entries.clear();
        self.entries.push(TuiEntry::new(
            EntryKind::System,
            format!("Session {} loaded.", short_id(&self.ctx.session_id)),
        ));
        for stored in &self.events {
            match &stored.event {
                holmes_core::event::Event::UserMessage { content, .. } => {
                    self.entries
                        .push(TuiEntry::new(EntryKind::User, content.clone()));
                }
                holmes_core::event::Event::Thinking { content, .. } => {
                    self.entries
                        .push(TuiEntry::new(EntryKind::Assistant, content.clone()));
                }
                holmes_core::event::Event::ToolCall {
                    name, arguments, ..
                } => {
                    self.entries.push(TuiEntry::new(
                        EntryKind::Tool,
                        format!("{name} started {arguments}"),
                    ));
                }
                holmes_core::event::Event::ToolResult {
                    name,
                    success,
                    content,
                    ..
                } => {
                    let mut text = format!(
                        "{} {} - {}",
                        name,
                        if *success { "ok" } else { "failed" },
                        folded_tool_output_summary(content)
                    );
                    if self.show_tool_output && !content.trim().is_empty() {
                        text.push('\n');
                        text.push_str(content.trim());
                    }
                    self.entries.push(TuiEntry::new(EntryKind::Tool, text));
                }
                holmes_core::event::Event::ToolBlocked {
                    tool_name, reason, ..
                } => self.entries.push(TuiEntry::new(
                    EntryKind::Permission,
                    format!("{tool_name} blocked - {reason}"),
                )),
                _ => {}
            }
        }
    }

    fn resolve_session_arg(&self, arg: &str) -> Option<String> {
        if let Ok(index) = arg.parse::<usize>() {
            if index > 0 {
                return self.sessions.get(index - 1).map(|s| s.id.clone());
            }
        }
        self.sessions
            .iter()
            .find(|s| s.id.starts_with(arg) || s.title.as_deref() == Some(arg))
            .map(|s| s.id.clone())
    }

    async fn push_session_status(&mut self) {
        match self.ctx.session_db.get_session(&self.ctx.session_id).await {
            Ok(Some(session)) => {
                self.entries.push(TuiEntry::new(
                    EntryKind::System,
                    format!(
                        "Session {}\nTitle: {}\nMode: {:?}\nMessages: {}\nTools: {}\nTokens: {} in / {} out",
                        short_id(&session.id),
                        session.title.as_deref().unwrap_or("(untitled)"),
                        session.mode,
                        session.message_count,
                        session.tool_call_count,
                        session.input_tokens,
                        session.output_tokens
                    ),
                ));
            }
            _ => self
                .entries
                .push(TuiEntry::new(EntryKind::Error, "Session info unavailable.")),
        }
    }

    fn push_dashboard(&mut self) {
        let dashboard = self
            .ctx
            .mind_palace
            .dashboard(&self.ctx.runtime_session.mode);
        if dashboard.sections.is_empty() {
            self.entries
                .push(TuiEntry::new(EntryKind::System, "Dashboard is empty."));
            return;
        }
        let mut text = String::new();
        for (_name, section) in dashboard.sections {
            text.push_str(&format!(
                "[{}]\n{}\n\n",
                section.title, section.content_summary
            ));
        }
        self.entries.push(TuiEntry::new(EntryKind::System, text));
    }

    fn push_tools(&mut self, arg: &str) {
        let defs = self.ctx.registry.definitions();
        if arg.is_empty() {
            let lines = defs
                .iter()
                .map(|tool| {
                    format!(
                        "{} - {}",
                        tool.function.name,
                        truncate_chars(&tool.function.description, 80)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.entries.push(TuiEntry::new(EntryKind::System, lines));
        } else if let Some(tool) = defs.iter().find(|tool| tool.function.name == arg) {
            self.entries.push(TuiEntry::new(
                EntryKind::System,
                format!(
                    "{}\n{}\n{}",
                    tool.function.name,
                    tool.function.description,
                    serde_json::to_string_pretty(&tool.function.parameters).unwrap_or_default()
                ),
            ));
        } else {
            self.status = format!("Tool not found: {arg}");
        }
    }

    async fn push_basic_info(&mut self, command: &str) {
        let text = match command {
            "provider" | "model" => self
                .ctx
                .config
                .llm
                .providers
                .iter()
                .map(|p| format!("{}: {} @ {}", p.name, p.model, p.base_url))
                .collect::<Vec<_>>()
                .join("\n"),
            "config" => format!(
                "Config: {}\nProviders: {}\nOutput: {}",
                self.ctx.data_dir.join("config.yaml").display(),
                self.ctx.config.llm.providers.len(),
                self.ctx.config.output_dir
            ),
            "usage" => match self.ctx.session_db.get_session(&self.ctx.session_id).await {
                Ok(Some(s)) => format!(
                    "Input: {}\nOutput: {}\nTotal: {}\nCost: ${:.4}",
                    s.input_tokens,
                    s.output_tokens,
                    s.input_tokens + s.output_tokens,
                    s.estimated_cost_usd
                ),
                _ => "Usage unavailable.".into(),
            },
            "workflows" => self.ctx.selector.workflow_names().join("\n"),
            _ => String::new(),
        };
        self.entries.push(TuiEntry::new(EntryKind::System, text));
    }

    fn cycle_permission_mode(&mut self, direction: i32) -> anyhow::Result<()> {
        let modes = [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::ReadOnly,
            PermissionMode::AcceptEdits,
            PermissionMode::DontAsk,
            PermissionMode::Bypass,
        ];
        let current = modes
            .iter()
            .position(|mode| mode == &self.ctx.config.permissions.mode)
            .unwrap_or(0);
        let next = if direction >= 0 {
            (current + 1) % modes.len()
        } else {
            (current + modes.len() - 1) % modes.len()
        };
        self.ctx.config.permissions.mode = modes[next].clone();
        save_config(&self.ctx)?;
        self.status = format!("Permission mode: {}.", self.ctx.config.permissions.mode);
        Ok(())
    }

    fn permission_row_count(&self) -> usize {
        4 + self.ctx.config.permissions.allowed_tools.len()
            + self.ctx.config.permissions.disallowed_tools.len()
    }

    fn remove_selected_permission_pattern(&mut self, selected: usize) -> anyhow::Result<()> {
        let allow_start = 2;
        let allow_len = self.ctx.config.permissions.allowed_tools.len();
        let deny_start = allow_start + allow_len + 1;
        let deny_len = self.ctx.config.permissions.disallowed_tools.len();

        if selected >= allow_start && selected < allow_start + allow_len {
            let idx = selected - allow_start;
            let removed = self.ctx.config.permissions.allowed_tools.remove(idx);
            save_config(&self.ctx)?;
            self.status = format!("Removed allowed pattern {removed}.");
        } else if selected >= deny_start && selected < deny_start + deny_len {
            let idx = selected - deny_start;
            let removed = self.ctx.config.permissions.disallowed_tools.remove(idx);
            save_config(&self.ctx)?;
            self.status = format!("Removed denied pattern {removed}.");
        }
        Ok(())
    }

    fn toggle_selected_guard(&mut self, selected: usize) -> anyhow::Result<()> {
        let rows = guard_rows();
        if let Some(row) = rows.get(selected) {
            let next = !self.guard_enabled(row.key);
            self.set_guard(row.key, next);
            refresh_guard_chain(&mut self.ctx);
            save_config(&self.ctx)?;
            self.status = format!(
                "Guard {} {}.",
                row.label,
                if next { "enabled" } else { "disabled" }
            );
        }
        Ok(())
    }

    fn guard_enabled(&self, key: &str) -> bool {
        match key {
            "immutable_field" => self.ctx.config.guards.immutable_field,
            "dangerous_command" => self.ctx.config.guards.dangerous_command,
            "repetition" => self.ctx.config.guards.repetition,
            "attack_surface" => self.ctx.config.guards.attack_surface,
            "evidence_extractor" => self.ctx.config.guards.evidence_extractor,
            "skeptic_gate" => self.ctx.config.guards.skeptic_gate,
            "failure_tracker" => self.ctx.config.guards.failure_tracker,
            "soft404" => self.ctx.config.guards.soft404,
            "read_state_seeding" => self.ctx.config.guards.read_state_seeding,
            _ => false,
        }
    }

    fn set_guard(&mut self, key: &str, value: bool) {
        match key {
            "immutable_field" => self.ctx.config.guards.immutable_field = value,
            "dangerous_command" => self.ctx.config.guards.dangerous_command = value,
            "repetition" => self.ctx.config.guards.repetition = value,
            "attack_surface" => self.ctx.config.guards.attack_surface = value,
            "evidence_extractor" => self.ctx.config.guards.evidence_extractor = value,
            "skeptic_gate" => self.ctx.config.guards.skeptic_gate = value,
            "failure_tracker" => self.ctx.config.guards.failure_tracker = value,
            "soft404" => self.ctx.config.guards.soft404 = value,
            "read_state_seeding" => self.ctx.config.guards.read_state_seeding = value,
            _ => {}
        }
    }

    fn move_cursor_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some((idx, _)) = self.input[..self.cursor].char_indices().last() {
            self.cursor = idx;
        }
    }

    fn move_cursor_right(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        if let Some((offset, ch)) = self.input[self.cursor..].char_indices().next() {
            self.cursor += offset + ch.len_utf8();
        }
    }

    fn insert_char(&mut self, ch: char) {
        self.input.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some((idx, _)) = self.input[..self.cursor].char_indices().last() {
            self.input.drain(idx..self.cursor);
            self.cursor = idx;
        }
    }

    fn delete_at_cursor(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        if let Some((_, ch)) = self.input[self.cursor..].char_indices().next() {
            let end = self.cursor + ch.len_utf8();
            self.input.drain(self.cursor..end);
        }
    }

    fn command_rows(&self, query: &str) -> Vec<(String, String)> {
        let q = query.to_ascii_lowercase();
        self.ctx
            .command_registry
            .all_command_hints()
            .into_iter()
            .filter(|(name, desc)| {
                q.is_empty()
                    || name.to_ascii_lowercase().contains(&q)
                    || desc.to_ascii_lowercase().contains(&q)
            })
            .collect()
    }

    fn session_rows(&self, query: &str) -> Vec<SessionSummary> {
        let q = query.to_ascii_lowercase();
        self.sessions
            .iter()
            .filter(|session| {
                q.is_empty()
                    || session.id.to_ascii_lowercase().contains(&q)
                    || session
                        .title
                        .as_deref()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .contains(&q)
                    || session
                        .preview
                        .as_deref()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .contains(&q)
            })
            .cloned()
            .collect()
    }

    fn tree_rows(&self, query: &str) -> Vec<TreeRow> {
        let q = query.to_ascii_lowercase();
        if !q.is_empty() {
            return self
                .session_rows(query)
                .into_iter()
                .map(|session| TreeRow {
                    id: session.id.clone(),
                    line: format!(
                        "{} {}",
                        short_id(&session.id),
                        session.title.as_deref().unwrap_or("(untitled)")
                    ),
                })
                .collect();
        }

        let mut children: BTreeMap<Option<String>, Vec<usize>> = BTreeMap::new();
        for (idx, session) in self.sessions.iter().enumerate() {
            children
                .entry(session.parent_session_id.clone())
                .or_default()
                .push(idx);
        }
        for indexes in children.values_mut() {
            indexes.sort_by_key(|idx| {
                std::cmp::Reverse(
                    self.sessions[*idx]
                        .last_active
                        .unwrap_or(self.sessions[*idx].started_at),
                )
            });
        }

        let mut rows = Vec::new();
        let mut visited = BTreeSet::new();
        if let Some(roots) = children.get(&None) {
            for (pos, idx) in roots.iter().enumerate() {
                self.push_tree_row(
                    &children,
                    *idx,
                    "",
                    pos + 1 == roots.len(),
                    &mut visited,
                    &mut rows,
                );
            }
        }
        for idx in 0..self.sessions.len() {
            if !visited.contains(&self.sessions[idx].id) {
                self.push_tree_row(&children, idx, "", true, &mut visited, &mut rows);
            }
        }
        rows
    }

    fn push_tree_row(
        &self,
        children: &BTreeMap<Option<String>, Vec<usize>>,
        idx: usize,
        prefix: &str,
        is_last: bool,
        visited: &mut BTreeSet<String>,
        rows: &mut Vec<TreeRow>,
    ) {
        let session = &self.sessions[idx];
        if !visited.insert(session.id.clone()) {
            return;
        }
        let has_children = children
            .get(&Some(session.id.clone()))
            .map(|items| !items.is_empty())
            .unwrap_or(false);
        let collapsed = self.collapsed_sessions.contains(&session.id);
        let connector = if prefix.is_empty() {
            ""
        } else if is_last {
            "`- "
        } else {
            "|- "
        };
        let marker = if session.id == self.ctx.session_id {
            ">"
        } else {
            " "
        };
        let fold = if has_children {
            if collapsed {
                "+"
            } else {
                "-"
            }
        } else {
            " "
        };
        let line = format!(
            "{}{}{}{} {} {:?} {} {}",
            prefix,
            connector,
            marker,
            fold,
            short_id(&session.id),
            session.mode,
            format_relative_time(session.last_active.unwrap_or(session.started_at)),
            session.title.as_deref().unwrap_or("(untitled)")
        );
        rows.push(TreeRow {
            id: session.id.clone(),
            line,
        });
        if collapsed {
            return;
        }
        let child_prefix = if prefix.is_empty() {
            String::new()
        } else if is_last {
            format!("{prefix}   ")
        } else {
            format!("{prefix}|  ")
        };
        if let Some(indexes) = children.get(&Some(session.id.clone())) {
            for (pos, child_idx) in indexes.iter().enumerate() {
                self.push_tree_row(
                    children,
                    *child_idx,
                    &child_prefix,
                    pos + 1 == indexes.len(),
                    visited,
                    rows,
                );
            }
        }
    }

    fn event_rows(&self, query: &str) -> Vec<EventRow> {
        let q = query.to_ascii_lowercase();
        self.events
            .iter()
            .filter_map(|event| {
                let label = event_type_label(&event.event);
                let summary = event_summary(&event.event);
                if q.is_empty()
                    || label.to_ascii_lowercase().contains(&q)
                    || summary.to_ascii_lowercase().contains(&q)
                {
                    Some(EventRow {
                        event_index: event.event_index,
                        label: label.into(),
                        summary,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    fn render(&mut self, stdout: &mut Stdout) -> anyhow::Result<()> {
        let (width, height) = terminal::size()?;
        queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
        self.render_header(stdout, width)?;
        let input_height = self
            .input_height(width)
            .min(height.saturating_sub(5))
            .max(3);
        let footer_height = 2;
        let chat_top = 2;
        let chat_height = height.saturating_sub(chat_top + input_height + footer_height);
        self.render_chat(stdout, width, chat_top, chat_height)?;
        self.render_input(
            stdout,
            width,
            height.saturating_sub(input_height + footer_height),
            input_height,
        )?;
        self.render_footer(
            stdout,
            width,
            height.saturating_sub(footer_height),
            footer_height,
        )?;
        self.render_overlay(stdout, width, height)?;
        self.render_cursor(stdout, width, height, input_height)?;
        stdout.flush()?;
        Ok(())
    }

    fn render_header(&self, stdout: &mut Stdout, width: u16) -> anyhow::Result<()> {
        let title = format!(
            " Holmes TUI  session={}  mode={:?}  permission={}{}",
            short_id(&self.ctx.session_id),
            self.ctx.runtime_session.mode,
            self.ctx.config.permissions.mode,
            if self.busy { "  working" } else { "" }
        );
        write_line(stdout, 0, 0, width, &title, Color::Cyan)?;
        write_line(
            stdout,
            0,
            1,
            width,
            &"-".repeat(width as usize),
            Color::DarkGrey,
        )?;
        Ok(())
    }

    fn render_chat(
        &self,
        stdout: &mut Stdout,
        width: u16,
        top: u16,
        height: u16,
    ) -> anyhow::Result<()> {
        let lines = flatten_entries(
            &self.entries,
            width.saturating_sub(CHAT_MARGIN * 2) as usize,
        );
        let visible = height as usize;
        let end = lines.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(visible);
        for (idx, line) in lines[start..end].iter().enumerate() {
            write_line(
                stdout,
                CHAT_MARGIN,
                top + idx as u16,
                width.saturating_sub(CHAT_MARGIN * 2),
                &line.text,
                line.color,
            )?;
        }
        Ok(())
    }

    fn input_height(&self, width: u16) -> u16 {
        let usable = width.saturating_sub(12).max(10) as usize;
        let lines = wrap_plain(&self.input, usable).len().max(1);
        (lines as u16 + 2).clamp(3, 8)
    }

    fn render_input(
        &self,
        stdout: &mut Stdout,
        width: u16,
        top: u16,
        height: u16,
    ) -> anyhow::Result<()> {
        write_line(
            stdout,
            0,
            top,
            width,
            &"-".repeat(width as usize),
            Color::DarkGrey,
        )?;
        let prompt = if self.busy { "Holmes " } else { "Watson " };
        let body_width = width.saturating_sub(10).max(10) as usize;
        let mut lines = wrap_plain(&self.input, body_width);
        if lines.is_empty() {
            lines.push(String::new());
        }
        for row in 0..height.saturating_sub(1) {
            let text = if row == 0 {
                format!(
                    "{prompt}> {}",
                    lines.get(row as usize).cloned().unwrap_or_default()
                )
            } else {
                format!(
                    "        {}",
                    lines.get(row as usize).cloned().unwrap_or_default()
                )
            };
            write_line(stdout, 0, top + 1 + row, width, &text, Color::White)?;
        }
        Ok(())
    }

    fn render_footer(
        &self,
        stdout: &mut Stdout,
        width: u16,
        top: u16,
        _height: u16,
    ) -> anyhow::Result<()> {
        write_line(stdout, 0, top, width, &self.footer_line(), Color::DarkGrey)?;
        write_line(stdout, 0, top + 1, width, &self.status, Color::Yellow)?;
        Ok(())
    }

    fn footer_line(&self) -> String {
        let tokens = format!(
            "{} in / {} out",
            self.ctx.runtime_session.tokens.input, self.ctx.runtime_session.tokens.output
        );
        let goal = self
            .ctx
            .runtime_state
            .active_goal
            .as_deref()
            .map(|goal| format!(" goal={}", truncate_chars(goal, 30)))
            .unwrap_or_default();
        format!(
            "Ctrl+L commands  Ctrl+N new  Ctrl+B fork  Ctrl+O tools={}  PgUp/PgDn scroll  {}{}",
            if self.show_tool_output {
                "full"
            } else {
                "folded"
            },
            tokens,
            goal
        )
    }

    fn render_overlay(&self, stdout: &mut Stdout, width: u16, height: u16) -> anyhow::Result<()> {
        match &self.overlay {
            Overlay::None => Ok(()),
            Overlay::Help => self.render_help(stdout, width, height),
            Overlay::CommandPalette(state) => {
                self.render_command_palette(stdout, width, height, state)
            }
            Overlay::Sessions(state) => self.render_sessions(stdout, width, height, state),
            Overlay::Tree(state) => self.render_tree(stdout, width, height, state),
            Overlay::Events(state) => self.render_events(stdout, width, height, state),
            Overlay::Permissions(state) => self.render_permissions(stdout, width, height, state),
            Overlay::Guards(state) => self.render_guards(stdout, width, height, state),
            Overlay::Prompt(prompt) => self.render_prompt(stdout, width, height, prompt),
        }
    }

    fn render_help(&self, stdout: &mut Stdout, width: u16, height: u16) -> anyhow::Result<()> {
        let lines = vec![
            "Holmes TUI".to_string(),
            "".to_string(),
            "F1 help              close this panel".to_string(),
            "F2 tree              searchable session tree; Enter resumes; f forks selected"
                .to_string(),
            "F3 events            event timeline; Enter forks from selected event".to_string(),
            "F4 permissions       mode, allow/deny lists, read-only auto approval".to_string(),
            "F5 guards            toggle GuardChain checks and repetition window".to_string(),
            "F6 sessions          flat recent-session selector".to_string(),
            "Ctrl+L               command palette".to_string(),
            "Ctrl+N               new session".to_string(),
            "Ctrl+B               fork current session at latest event".to_string(),
            "Ctrl+O               fold/expand tool output".to_string(),
            "Alt+Enter            newline in editor".to_string(),
            "Esc                  close overlay".to_string(),
            "Ctrl+D / Ctrl+C      exit when editor is empty".to_string(),
        ];
        draw_box(stdout, width, height, "Help", &lines, None)
    }

    fn render_command_palette(
        &self,
        stdout: &mut Stdout,
        width: u16,
        height: u16,
        state: &SelectorOverlay,
    ) -> anyhow::Result<()> {
        let rows = self.command_rows(&state.query);
        let mut lines = vec![format!("Search: {}", state.query)];
        for (idx, (name, desc)) in rows.iter().take(18).enumerate() {
            lines.push(format!(
                "{} {:<18} {}",
                if idx == state.selected { ">" } else { " " },
                name,
                desc
            ));
        }
        draw_box(
            stdout,
            width,
            height,
            "Command Palette",
            &lines,
            Some("Enter inserts command"),
        )
    }

    fn render_sessions(
        &self,
        stdout: &mut Stdout,
        width: u16,
        height: u16,
        state: &SelectorOverlay,
    ) -> anyhow::Result<()> {
        let rows = self.session_rows(&state.query);
        let mut lines = vec![format!("Search: {}", state.query)];
        for (idx, session) in rows.iter().take(18).enumerate() {
            lines.push(format!(
                "{} {} {:?} {:<10} {}",
                if idx == state.selected { ">" } else { " " },
                short_id(&session.id),
                session.mode,
                format_relative_time(session.last_active.unwrap_or(session.started_at)),
                session.title.as_deref().unwrap_or("(untitled)")
            ));
        }
        draw_box(
            stdout,
            width,
            height,
            "Sessions",
            &lines,
            Some("Enter resumes"),
        )
    }

    fn render_tree(
        &self,
        stdout: &mut Stdout,
        width: u16,
        height: u16,
        state: &SelectorOverlay,
    ) -> anyhow::Result<()> {
        let rows = self.tree_rows(&state.query);
        let mut lines = vec![format!("Search: {}", state.query)];
        for (idx, row) in rows.iter().take(22).enumerate() {
            lines.push(format!(
                "{} {}",
                if idx == state.selected { ">" } else { " " },
                row.line
            ));
        }
        draw_box(
            stdout,
            width,
            height,
            "Session Tree",
            &lines,
            Some("Enter resumes  f forks selected  Left/Right fold"),
        )
    }

    fn render_events(
        &self,
        stdout: &mut Stdout,
        width: u16,
        height: u16,
        state: &EventOverlay,
    ) -> anyhow::Result<()> {
        let rows = self.event_rows(&state.query);
        let mut lines = vec![format!("Search: {}", state.query)];
        let max = 22usize;
        let start = state.selected.saturating_sub(max.saturating_sub(1));
        for (offset, row) in rows.iter().skip(start).take(max).enumerate() {
            let idx = start + offset;
            lines.push(format!(
                "{} {:>4} {:<18} {}",
                if idx == state.selected { ">" } else { " " },
                row.event_index,
                row.label,
                truncate_chars(&row.summary, 80)
            ));
        }
        draw_box(
            stdout,
            width,
            height,
            "Events",
            &lines,
            Some("Enter/f forks from event"),
        )
    }

    fn render_permissions(
        &self,
        stdout: &mut Stdout,
        width: u16,
        height: u16,
        state: &PermissionsOverlay,
    ) -> anyhow::Result<()> {
        let mut lines = Vec::new();
        lines.push(format!(
            "{} Mode: {}",
            mark(state.selected, 0),
            self.ctx.config.permissions.mode
        ));
        lines.push(format!(
            "{} Auto approve read-only: {}",
            mark(state.selected, 1),
            if self.ctx.config.permissions.auto_approve_read_only {
                "on"
            } else {
                "off"
            }
        ));
        let mut row = 2;
        if self.ctx.config.permissions.allowed_tools.is_empty() {
            lines.push(format!("{} Allowed: (empty)", mark(state.selected, row)));
            row += 1;
        } else {
            for pattern in &self.ctx.config.permissions.allowed_tools {
                lines.push(format!("{} Allow {}", mark(state.selected, row), pattern));
                row += 1;
            }
        }
        if self.ctx.config.permissions.disallowed_tools.is_empty() {
            lines.push(format!("{} Denied: (empty)", mark(state.selected, row)));
        } else {
            for pattern in &self.ctx.config.permissions.disallowed_tools {
                lines.push(format!("{} Deny  {}", mark(state.selected, row), pattern));
                row += 1;
            }
        }
        lines.push("".into());
        lines.push("Left/Right cycle mode  Space toggles auto-read-only  a allow  d deny  x remove  r reset".into());
        draw_box(stdout, width, height, "Permission Policy", &lines, None)
    }

    fn render_guards(
        &self,
        stdout: &mut Stdout,
        width: u16,
        height: u16,
        state: &GuardsOverlay,
    ) -> anyhow::Result<()> {
        let mut lines = Vec::new();
        for (idx, row) in guard_rows().iter().enumerate() {
            lines.push(format!(
                "{} {:<20} {:<3} {}",
                mark(state.selected, idx),
                row.label,
                if self.guard_enabled(row.key) {
                    "on"
                } else {
                    "off"
                },
                row.description
            ));
        }
        let window_row = guard_rows().len();
        lines.push(format!(
            "{} repetition-window  {}",
            mark(state.selected, window_row),
            self.ctx.config.guards.repetition_window
        ));
        lines.push("".into());
        lines.push("Space toggles  a toggles all  +/- changes repetition window".into());
        draw_box(stdout, width, height, "GuardChain", &lines, None)
    }

    fn render_prompt(
        &self,
        stdout: &mut Stdout,
        width: u16,
        height: u16,
        prompt: &TextPrompt,
    ) -> anyhow::Result<()> {
        let lines = vec![
            prompt.label.clone(),
            prompt.value.clone(),
            "Enter confirms  Esc cancels".into(),
        ];
        draw_box(stdout, width, height, &prompt.title, &lines, None)
    }

    fn render_cursor(
        &self,
        stdout: &mut Stdout,
        width: u16,
        height: u16,
        input_height: u16,
    ) -> anyhow::Result<()> {
        if self.busy || !matches!(self.overlay, Overlay::None) {
            return Ok(());
        }
        let body_width = width.saturating_sub(12).max(10) as usize;
        let before = &self.input[..self.cursor.min(self.input.len())];
        let line = before.chars().count() / body_width;
        let col = before.chars().count() % body_width;
        let y = height
            .saturating_sub(input_height + 2)
            .saturating_add(1 + line as u16);
        let x = 9 + col as u16;
        queue!(stdout, MoveTo(x.min(width.saturating_sub(1)), y))?;
        Ok(())
    }
}

struct TuiRuntimeSink<'a> {
    entries: &'a mut Vec<TuiEntry>,
    stdout: &'a mut Stdout,
    show_tool_output: bool,
    footer: String,
}

impl RuntimeSink for TuiRuntimeSink<'_> {
    fn emit(&mut self, event: StreamEvent) {
        match event.data {
            RuntimeYield::MessageToUser { content }
            | RuntimeYield::PlanUpdate { content }
            | RuntimeYield::FinalAnswer { content, .. } => {
                self.entries
                    .push(TuiEntry::new(EntryKind::Assistant, content));
            }
            RuntimeYield::ToolStarted { name, call_id } => {
                let suffix = call_id
                    .as_deref()
                    .map(|id| format!(" ({})", short_id(id)))
                    .unwrap_or_default();
                self.entries.push(TuiEntry::new(
                    EntryKind::Tool,
                    format!("{name} started{suffix}"),
                ));
            }
            RuntimeYield::PermissionDecision {
                tool_name,
                allowed,
                reason,
                ..
            } => {
                if !allowed || self.show_tool_output {
                    self.entries.push(TuiEntry::new(
                        EntryKind::Permission,
                        format!(
                            "{} {} - {}",
                            tool_name,
                            if allowed { "allowed" } else { "blocked" },
                            reason
                        ),
                    ));
                }
            }
            RuntimeYield::ToolFinished {
                name,
                success,
                content,
                ..
            } => {
                let mut text = format!(
                    "{} {} - {}",
                    name,
                    if success { "ok" } else { "failed" },
                    folded_tool_output_summary(&content)
                );
                if self.show_tool_output && !content.trim().is_empty() {
                    text.push('\n');
                    text.push_str(content.trim());
                }
                self.entries.push(TuiEntry::new(EntryKind::Tool, text));
            }
            RuntimeYield::EvidenceUpdate { content } => {
                self.entries
                    .push(TuiEntry::new(EntryKind::Evidence, content));
            }
            RuntimeYield::NeedsUserInput { prompt } => {
                self.entries
                    .push(TuiEntry::new(EntryKind::Assistant, prompt));
            }
            RuntimeYield::CompactionBoundary {
                before_count,
                after_count,
                method,
                ..
            } => self.entries.push(TuiEntry::new(
                EntryKind::System,
                format!("Context compacted {before_count} -> {after_count} ({method})"),
            )),
            RuntimeYield::Error { message } => {
                self.entries.push(TuiEntry::new(EntryKind::Error, message));
            }
        }
        let _ = render_runtime_snapshot(self.stdout, self.entries, &self.footer);
    }
}

#[derive(Clone)]
struct TreeRow {
    id: String,
    line: String,
}

#[derive(Clone)]
struct EventRow {
    event_index: u64,
    label: String,
    summary: String,
}

struct GuardRow {
    key: &'static str,
    label: &'static str,
    description: &'static str,
}

fn guard_rows() -> Vec<GuardRow> {
    vec![
        GuardRow {
            key: "immutable_field",
            label: "immutable-field",
            description: "protects scoped target state",
        },
        GuardRow {
            key: "dangerous_command",
            label: "dangerous-command",
            description: "blocks destructive shell actions",
        },
        GuardRow {
            key: "repetition",
            label: "repetition",
            description: "stops repeated low-value loops",
        },
        GuardRow {
            key: "attack_surface",
            label: "attack-surface",
            description: "extracts hosts, ports, endpoints",
        },
        GuardRow {
            key: "evidence_extractor",
            label: "evidence-extractor",
            description: "extracts evidence bundles",
        },
        GuardRow {
            key: "skeptic_gate",
            label: "skeptic-gate",
            description: "keeps weak findings tentative",
        },
        GuardRow {
            key: "failure_tracker",
            label: "failure-tracker",
            description: "tracks failed actions",
        },
        GuardRow {
            key: "soft404",
            label: "soft404",
            description: "detects false-positive HTTP probes",
        },
        GuardRow {
            key: "read_state_seeding",
            label: "read-state-seeding",
            description: "tracks read/write state safely",
        },
    ]
}

fn render_runtime_snapshot(
    stdout: &mut Stdout,
    entries: &[TuiEntry],
    footer: &str,
) -> anyhow::Result<()> {
    let (width, height) = terminal::size()?;
    let chat_height = height.saturating_sub(5);
    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
    write_line(stdout, 0, 0, width, " Holmes TUI  working", Color::Cyan)?;
    write_line(
        stdout,
        0,
        1,
        width,
        &"-".repeat(width as usize),
        Color::DarkGrey,
    )?;
    let lines = flatten_entries(entries, width.saturating_sub(CHAT_MARGIN * 2) as usize);
    let start = lines.len().saturating_sub(chat_height as usize);
    for (idx, line) in lines[start..].iter().enumerate() {
        write_line(
            stdout,
            CHAT_MARGIN,
            2 + idx as u16,
            width.saturating_sub(CHAT_MARGIN * 2),
            &line.text,
            line.color,
        )?;
    }
    write_line(
        stdout,
        0,
        height.saturating_sub(2),
        width,
        footer,
        Color::DarkGrey,
    )?;
    write_line(
        stdout,
        0,
        height.saturating_sub(1),
        width,
        "Holmes is working...",
        Color::Yellow,
    )?;
    stdout.flush()?;
    Ok(())
}

struct RenderLine {
    text: String,
    color: Color,
}

fn flatten_entries(entries: &[TuiEntry], width: usize) -> Vec<RenderLine> {
    let mut lines = Vec::new();
    for entry in entries {
        let (prefix, color) = match entry.kind {
            EntryKind::User => ("Watson: ", Color::Green),
            EntryKind::Assistant => ("Holmes: ", Color::White),
            EntryKind::Tool => ("Tool:   ", Color::Blue),
            EntryKind::Permission => ("Policy: ", Color::Magenta),
            EntryKind::Evidence => ("Evidence:", Color::Yellow),
            EntryKind::System => ("System: ", Color::DarkGrey),
            EntryKind::Error => ("Error:  ", Color::Red),
        };
        let body_width = width.saturating_sub(prefix.len()).max(8);
        let wrapped = wrap_plain(entry.text.trim(), body_width);
        if wrapped.is_empty() {
            lines.push(RenderLine {
                text: prefix.into(),
                color,
            });
        } else {
            for (idx, line) in wrapped.into_iter().enumerate() {
                let text = if idx == 0 {
                    format!("{prefix}{line}")
                } else {
                    format!("{}{}", " ".repeat(prefix.len()), line)
                };
                lines.push(RenderLine { text, color });
            }
        }
        lines.push(RenderLine {
            text: String::new(),
            color: Color::Reset,
        });
    }
    lines
}

fn wrap_plain(input: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw_line in input.lines() {
        if raw_line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in raw_line.split_whitespace() {
            let extra = if current.is_empty() { 0 } else { 1 };
            if current.chars().count() + word.chars().count() + extra > width && !current.is_empty()
            {
                out.push(current);
                current = String::new();
            }
            if !current.is_empty() {
                current.push(' ');
            }
            if word.chars().count() > width {
                for chunk in chunk_chars(word, width) {
                    if !current.is_empty() {
                        out.push(current);
                        current = String::new();
                    }
                    out.push(chunk);
                }
            } else {
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out
}

fn chunk_chars(input: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        if current.chars().count() >= width {
            out.push(current);
            current = String::new();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn draw_box(
    stdout: &mut Stdout,
    width: u16,
    height: u16,
    title: &str,
    lines: &[String],
    footer: Option<&str>,
) -> anyhow::Result<()> {
    let box_width = width.saturating_sub(8).clamp(32, 110);
    let box_height = (lines.len() as u16 + 4)
        .min(height.saturating_sub(4))
        .max(8);
    let left = width.saturating_sub(box_width) / 2;
    let top = height.saturating_sub(box_height) / 2;
    let inner = box_width.saturating_sub(4);

    write_line(
        stdout,
        left,
        top,
        box_width,
        &format!("+{}+", "-".repeat(box_width.saturating_sub(2) as usize)),
        Color::Cyan,
    )?;
    write_line(
        stdout,
        left,
        top + 1,
        box_width,
        &format!("| {:<width$} |", title, width = inner as usize),
        Color::Cyan,
    )?;
    write_line(
        stdout,
        left,
        top + 2,
        box_width,
        &format!("+{}+", "-".repeat(box_width.saturating_sub(2) as usize)),
        Color::Cyan,
    )?;

    let visible_lines = box_height.saturating_sub(5) as usize;
    for (idx, line) in lines.iter().take(visible_lines).enumerate() {
        write_line(
            stdout,
            left,
            top + 3 + idx as u16,
            box_width,
            &format!(
                "| {:<width$} |",
                truncate_chars(line, inner as usize),
                width = inner as usize
            ),
            Color::White,
        )?;
    }
    for row in lines.len().min(visible_lines)..visible_lines {
        write_line(
            stdout,
            left,
            top + 3 + row as u16,
            box_width,
            &format!("| {:<width$} |", "", width = inner as usize),
            Color::White,
        )?;
    }
    let footer_text = footer.unwrap_or("Esc closes");
    write_line(
        stdout,
        left,
        top + box_height.saturating_sub(2),
        box_width,
        &format!(
            "| {:<width$} |",
            truncate_chars(footer_text, inner as usize),
            width = inner as usize
        ),
        Color::DarkGrey,
    )?;
    write_line(
        stdout,
        left,
        top + box_height.saturating_sub(1),
        box_width,
        &format!("+{}+", "-".repeat(box_width.saturating_sub(2) as usize)),
        Color::Cyan,
    )?;
    Ok(())
}

fn write_line(
    stdout: &mut Stdout,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    color: Color,
) -> anyhow::Result<()> {
    let mut s = truncate_chars(text, width as usize);
    let len = s.chars().count();
    if len < width as usize {
        s.push_str(&" ".repeat(width as usize - len));
    }
    queue!(
        stdout,
        MoveTo(x, y),
        SetForegroundColor(color),
        Print(s),
        ResetColor
    )?;
    Ok(())
}

fn mark(selected: usize, row: usize) -> &'static str {
    if selected == row {
        ">"
    } else {
        " "
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}
