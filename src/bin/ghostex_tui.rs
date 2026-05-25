use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use serde::Deserialize;

const POLL_INTERVAL: Duration = Duration::from_millis(16);
const SESSION_LIST_REFRESH: Duration = Duration::from_secs(3);
const GHOSTEX_TUI_TERM: &str = "xterm-256color";
const GHOSTEX_TUI_COLORTERM: &str = "truecolor";

#[derive(Debug, Deserialize, Clone)]
struct SessionItem {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default, rename = "projectId")]
    project_id: Option<String>,
    #[serde(default, rename = "projectName")]
    project_name: Option<String>,
    #[serde(default, rename = "projectPath")]
    project_path: Option<String>,
    #[serde(default, rename = "sessionId")]
    session_id: String,
    #[serde(default)]
    title: String,
}

#[derive(Debug, Deserialize)]
struct SessionListResult {
    #[serde(default)]
    sessions: Vec<SessionItem>,
}

#[derive(Debug, Clone)]
struct ProjectGroup {
    name: String,
    sessions: Vec<SessionItem>,
}

#[derive(Debug, Clone)]
enum SwitchRow {
    Project(String),
    Session(SessionItem),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Attached,
    Switcher,
}

struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
}

impl PtySession {
    fn spawn(session: &SessionItem, area: Rect) -> io::Result<Self> {
        let shell_command = format!(
            "{} attach --session-id {}",
            ghostex_cli_command(),
            shell_quote(&session.session_id)
        );
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: area.height.max(1),
                cols: area.width.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_other)?;
        let mut command = CommandBuilder::new("/bin/zsh");
        command.arg("-lc");
        command.arg(shell_command);
        /*
        CDXC:GhostexTui 2026-05-25-15:50:
        The attached session PTY is rendered by Ghostex TUI, not by the outer
        shell that launched `gtx`. Force a real terminal identity so Codex CLI,
        Starship, and other terminal-aware tools do not inherit TERM=dumb from
        desktop launchers or non-terminal hosts.
        */
        command.env("TERM", GHOSTEX_TUI_TERM);
        command.env("COLORTERM", GHOSTEX_TUI_COLORTERM);
        command.env("TERM_PROGRAM", "ghostex-tui");
        let child = pair.slave.spawn_command(command).map_err(io_other)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(io_other)?;
        let writer = pair.master.take_writer().map_err(io_other)?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if tx.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let parser = vt100::Parser::new(area.height.max(1), area.width.max(1), 0);
        Ok(Self {
            master: pair.master,
            child,
            writer,
            rx,
            parser,
        })
    }

    fn resize(&mut self, area: Rect) {
        let _ = self.master.resize(PtySize {
            rows: area.height.max(1),
            cols: area.width.max(1),
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.set_size(area.height.max(1), area.width.max(1));
    }

    fn drain_output(&mut self) {
        while let Ok(bytes) = self.rx.try_recv() {
            self.parser.process(&bytes);
        }
    }

    fn write_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

struct App {
    groups: Vec<ProjectGroup>,
    rows: Vec<SwitchRow>,
    selected_session_index: usize,
    selected_row_index: usize,
    active_session: Option<SessionItem>,
    pty: Option<PtySession>,
    mode: Mode,
    switch_scroll: usize,
    last_refresh: Instant,
    status: String,
}

impl App {
    fn new(area: Rect) -> Self {
        let mut app = Self {
            groups: Vec::new(),
            rows: Vec::new(),
            selected_session_index: 0,
            selected_row_index: 0,
            active_session: None,
            pty: None,
            mode: Mode::Switcher,
            switch_scroll: 0,
            last_refresh: Instant::now() - SESSION_LIST_REFRESH,
            status: String::new(),
        };
        app.refresh_sessions();
        /*
        CDXC:GhostexTui 2026-05-25-15:11:
        Bare `gtx` should open on the project/session switcher, not auto-attach
        the first sidebar session. Session PTYs are spawned only after the user
        clicks a row or presses Enter/Space, so saved resume-command output from
        an arbitrary first session cannot appear as launch errors.
        */
        let _ = area;
        app
    }

    fn refresh_sessions(&mut self) {
        match fetch_sessions() {
            Ok(sessions) => {
                self.groups = group_sessions(sessions);
                self.rows = switch_rows(&self.groups);
                self.selected_row_index =
                    self.row_index_for_session_index(self.selected_session_index);
                self.last_refresh = Instant::now();
                if self.groups.is_empty() {
                    self.status = "No Ghostex sessions found.".to_string();
                }
            }
            Err(err) => {
                self.status = format!("Could not load Ghostex sessions: {err}");
            }
        }
    }

    fn maybe_refresh_sessions(&mut self) {
        if self.mode == Mode::Switcher && self.last_refresh.elapsed() >= SESSION_LIST_REFRESH {
            self.refresh_sessions();
        }
    }

    fn attach(&mut self, session: SessionItem, area: Rect) {
        match PtySession::spawn(&session, area) {
            Ok(pty) => {
                self.pty = Some(pty);
                self.status.clear();
                self.active_session = Some(session);
                self.mode = Mode::Attached;
            }
            Err(err) => {
                self.status = format!("Could not attach session: {err}");
                self.mode = Mode::Switcher;
            }
        }
    }

    fn session_at(&self, session_index: usize) -> Option<&SessionItem> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                SwitchRow::Session(session) => Some(session),
                SwitchRow::Project(_) => None,
            })
            .nth(session_index)
    }

    fn session_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row, SwitchRow::Session(_)))
            .count()
    }

    fn row_index_for_session_index(&self, session_index: usize) -> usize {
        let mut seen = 0usize;
        for (idx, row) in self.rows.iter().enumerate() {
            if matches!(row, SwitchRow::Session(_)) {
                if seen == session_index {
                    return idx;
                }
                seen += 1;
            }
        }
        0
    }

    fn select_delta(&mut self, delta: isize) {
        let count = self.session_count();
        if count == 0 {
            return;
        }
        let next = wrap_index(self.selected_session_index as isize + delta, count);
        self.selected_session_index = next;
        self.selected_row_index = self.row_index_for_session_index(next);
    }

    fn select_row_at_document_y(&mut self, doc_y: usize) -> Option<SessionItem> {
        let row = self.rows.get(doc_y)?.clone();
        let SwitchRow::Session(session) = row else {
            return None;
        };
        self.selected_row_index = doc_y;
        self.selected_session_index = self
            .rows
            .iter()
            .take(doc_y + 1)
            .filter(|row| matches!(row, SwitchRow::Session(_)))
            .count()
            .saturating_sub(1);
        Some(session)
    }

    fn switcher_max_scroll(&self, viewport: Rect) -> usize {
        self.rows.len().saturating_sub(viewport.height as usize)
    }

    fn keep_selected_visible(&mut self, viewport: Rect) {
        let row = self.selected_row_index;
        if row < self.switch_scroll {
            self.switch_scroll = row;
        } else if row >= self.switch_scroll + viewport.height as usize {
            self.switch_scroll = row
                .saturating_sub(viewport.height as usize)
                .saturating_add(1);
        }
        self.switch_scroll = self.switch_scroll.min(self.switcher_max_scroll(viewport));
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn main() -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let initial_area = rect_from_size(terminal.size()?);
    let initial_terminal = terminal_area(initial_area);
    let mut app = App::new(initial_terminal);

    loop {
        let size = rect_from_size(terminal.size()?);
        let terminal_rect = terminal_area(size);
        if let Some(pty) = app.pty.as_mut() {
            pty.resize(terminal_rect);
            pty.drain_output();
        }
        app.maybe_refresh_sessions();
        terminal.draw(|frame| render(frame, &mut app))?;

        if event::poll(POLL_INTERVAL)? {
            match event::read()? {
                Event::Key(key) => {
                    if handle_key(&mut app, key, terminal_rect) {
                        break;
                    }
                }
                Event::Mouse(mouse) => handle_mouse(&mut app, mouse, size, terminal_rect),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
    Ok(())
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    render_header(frame, app, chunks[0]);
    match app.mode {
        Mode::Attached => render_terminal(frame, app, chunks[1]),
        Mode::Switcher => render_switcher(frame, app, chunks[1]),
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let switch_width = 12u16.min(area.width);
    let status_width = area.width.saturating_sub(switch_width);
    let status = Rect::new(area.x, area.y, status_width, area.height);
    let switch = switch_button_rect(area);
    let title = app
        .active_session
        .as_ref()
        .map(|session| session.title.as_str())
        .unwrap_or("No session");
    let project = app
        .active_session
        .as_ref()
        .map(project_label)
        .unwrap_or_else(|| "Ghostex".to_string());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " Ghostex ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(137, 180, 250))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                title.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ])),
        Rect::new(status.x, status.y, status.width, 1),
    );
    frame.render_widget(
        Paragraph::new(format!(" {project}")).style(Style::default().fg(Color::DarkGray)),
        Rect::new(status.x, status.y + 1, status.width, 1),
    );
    frame.render_widget(
        Paragraph::new("switch")
            .style(
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(49, 50, 68))
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(ratatui::layout::Alignment::Center)
            .block(Block::default().borders(Borders::LEFT)),
        switch,
    );
}

fn render_terminal(frame: &mut Frame, app: &mut App, area: Rect) {
    if let Some(pty) = app.pty.as_mut() {
        let screen = pty.parser.screen();
        /*
        CDXC:GhostexTui 2026-05-25-16:00:
        Ghostex TUI attaches through `zmx`, unlike Herdr's native pane runtime,
        but the PTY parser still stores ANSI attributes per cell. Render cells
        as styled spans instead of `screen.rows(...)` so Codex, Starship, and
        CLI color output keep foreground, background, truecolor, and text modes.
        */
        for row in 0..area.height {
            frame.render_widget(
                Paragraph::new(Line::from(render_terminal_row(screen, row, area.width))),
                Rect::new(area.x, area.y + row, area.width, 1),
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new(app.status.as_str()).style(Style::default().fg(Color::Red)),
            area,
        );
    }
}

fn render_terminal_row(screen: &vt100::Screen, row: u16, width: u16) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current_text = String::new();
    let mut current_style: Option<Style> = None;
    for col in 0..width {
        let Some(cell) = screen.cell(row, col) else {
            push_terminal_span(&mut spans, &mut current_text, &mut current_style);
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let style = terminal_cell_style(cell);
        if current_style.is_some_and(|existing| existing != style) {
            push_terminal_span(&mut spans, &mut current_text, &mut current_style);
        }
        current_style = Some(style);
        if cell.has_contents() {
            current_text.push_str(&cell.contents());
        } else {
            current_text.push(' ');
        }
    }
    push_terminal_span(&mut spans, &mut current_text, &mut current_style);
    spans
}

fn push_terminal_span(
    spans: &mut Vec<Span<'static>>,
    current_text: &mut String,
    current_style: &mut Option<Style>,
) {
    if current_text.is_empty() {
        return;
    }
    spans.push(Span::styled(
        std::mem::take(current_text),
        current_style.take().unwrap_or_default(),
    ));
}

fn terminal_cell_style(cell: &vt100::Cell) -> Style {
    let mut fg = terminal_color(cell.fgcolor());
    let mut bg = terminal_color(cell.bgcolor());
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    let mut style = Style::default().fg(fg).bg(bg);
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn terminal_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn render_switcher(frame: &mut Frame, app: &mut App, area: Rect) {
    frame.render_widget(Clear, area);
    app.keep_selected_visible(area);
    let visible_rows = app
        .rows
        .iter()
        .enumerate()
        .skip(app.switch_scroll)
        .take(area.height as usize)
        .map(|(idx, row)| match row {
            SwitchRow::Project(project) => ListItem::new(Line::from(Span::styled(
                project.clone(),
                Style::default()
                    .fg(Color::Rgb(137, 180, 250))
                    .add_modifier(Modifier::BOLD),
            ))),
            SwitchRow::Session(session) => {
                let selected = idx == app.selected_row_index;
                let bg = if selected {
                    Color::Rgb(49, 50, 68)
                } else {
                    Color::Reset
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("  [{}] ", agent_indicator(session)),
                        Style::default().fg(agent_color(session)).bg(bg),
                    ),
                    Span::styled(
                        session.title.clone(),
                        Style::default()
                            .fg(Color::White)
                            .bg(bg)
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ]))
                .style(Style::default().bg(bg))
            }
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    let selected_visible = app.selected_row_index.checked_sub(app.switch_scroll);
    if selected_visible.is_some_and(|idx| idx < visible_rows.len()) {
        state.select(selected_visible);
    }
    let list = List::new(visible_rows)
        .block(
            Block::default()
                .title(" switch session ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(137, 180, 250))),
        )
        .highlight_symbol(" ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn handle_key(app: &mut App, key: KeyEvent, terminal_rect: Rect) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
        return true;
    }
    match app.mode {
        Mode::Switcher => match key.code {
            KeyCode::Esc if app.active_session.is_some() => app.mode = Mode::Attached,
            KeyCode::Up => app.select_delta(-1),
            KeyCode::Down => app.select_delta(1),
            KeyCode::PageUp => app.select_delta(-5),
            KeyCode::PageDown => app.select_delta(5),
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(session) = app.session_at(app.selected_session_index).cloned() {
                    app.attach(session, terminal_rect);
                }
            }
            _ => {}
        },
        Mode::Attached => {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('s'))
            {
                app.mode = Mode::Switcher;
                app.refresh_sessions();
                return false;
            }
            if let Some(bytes) = encode_key(key) {
                if let Some(pty) = app.pty.as_mut() {
                    pty.write_input(&bytes);
                }
            }
        }
    }
    false
}

fn handle_mouse(app: &mut App, mouse: MouseEvent, full: Rect, terminal_rect: Rect) {
    match app.mode {
        Mode::Attached => {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                && rect_contains(
                    switch_button_rect(header_area(full)),
                    mouse.column,
                    mouse.row,
                )
            {
                app.mode = Mode::Switcher;
                app.refresh_sessions();
            }
        }
        Mode::Switcher => match mouse.kind {
            MouseEventKind::ScrollUp => app.select_delta(-1),
            MouseEventKind::ScrollDown => app.select_delta(1),
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.row < terminal_rect.y {
                    return;
                }
                let doc_y = app
                    .switch_scroll
                    .saturating_add(mouse.row.saturating_sub(terminal_rect.y) as usize);
                if let Some(session) = app.select_row_at_document_y(doc_y) {
                    app.attach(session, terminal_rect);
                }
            }
            _ => {}
        },
    }
}

fn fetch_sessions() -> io::Result<Vec<SessionItem>> {
    let output = Command::new("/bin/zsh")
        .arg("-lc")
        .arg(format!("{} sessions --json", ghostex_cli_command()))
        .output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let result: SessionListResult = serde_json::from_slice(&output.stdout)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(result.sessions)
}

fn io_other(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error.to_string())
}

fn group_sessions(sessions: Vec<SessionItem>) -> Vec<ProjectGroup> {
    let mut indexes = BTreeMap::<String, usize>::new();
    let mut groups = Vec::<ProjectGroup>::new();
    for session in sessions {
        let key = session
            .project_id
            .clone()
            .unwrap_or_else(|| project_label(&session));
        let idx = if let Some(idx) = indexes.get(&key).copied() {
            idx
        } else {
            let idx = groups.len();
            indexes.insert(key, idx);
            groups.push(ProjectGroup {
                name: if groups.is_empty()
                    && session.project_path.as_deref().unwrap_or("").is_empty()
                {
                    "Quick Terminals".to_string()
                } else {
                    project_label(&session)
                },
                sessions: Vec::new(),
            });
            idx
        };
        groups[idx].sessions.push(session);
    }
    groups
}

fn switch_rows(groups: &[ProjectGroup]) -> Vec<SwitchRow> {
    let mut rows = Vec::new();
    for group in groups {
        rows.push(SwitchRow::Project(group.name.clone()));
        rows.extend(group.sessions.iter().cloned().map(SwitchRow::Session));
    }
    rows
}

fn ghostex_cli_command() -> String {
    std::env::var("GHOSTEX_TUI_CLI_COMMAND").unwrap_or_else(|_| "gtx".to_string())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn header_area(full: Rect) -> Rect {
    Rect::new(full.x, full.y, full.width, full.height.min(2))
}

fn terminal_area(full: Rect) -> Rect {
    let header = header_area(full);
    Rect::new(
        full.x,
        full.y + header.height,
        full.width,
        full.height.saturating_sub(header.height),
    )
}

fn rect_from_size(size: ratatui::layout::Size) -> Rect {
    Rect::new(0, 0, size.width, size.height)
}

fn switch_button_rect(header: Rect) -> Rect {
    let width = 12u16.min(header.width);
    Rect::new(
        header.x + header.width.saturating_sub(width),
        header.y,
        width,
        header.height,
    )
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.x + rect.width
        && row >= rect.y
        && row < rect.y + rect.height
}

fn project_label(session: &SessionItem) -> String {
    session
        .project_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or(session.project_path.as_deref())
        .unwrap_or("Project")
        .to_string()
}

fn agent_indicator(session: &SessionItem) -> &'static str {
    match normalized_agent(session).as_str() {
        "antigravity" | "antigravity-cli" => "AGY",
        "claude" | "claude-code" => "CLD",
        "codex" | "codex-cli" | "work-codex" => "CDX",
        "copilot" => "PLT",
        "cursor" | "cursor-cli" => "CRS",
        "gemini" => "GEM",
        "grok" | "grok-build" => "GRK",
        "pi" => "PIA",
        "t3" | "t3-code" => "T3C",
        _ => "UNK",
    }
}

fn agent_color(session: &SessionItem) -> Color {
    match normalized_agent(session).as_str() {
        "antigravity" | "antigravity-cli" | "cursor" | "cursor-cli" => Color::Rgb(116, 155, 255),
        "claude" | "claude-code" => Color::Rgb(217, 119, 87),
        "codex" | "codex-cli" | "work-codex" => Color::Rgb(169, 145, 255),
        "gemini" => Color::Rgb(139, 154, 255),
        "pi" => Color::Rgb(200, 255, 98),
        "t3" | "t3-code" => Color::Rgb(255, 106, 243),
        _ => Color::White,
    }
}

fn normalized_agent(session: &SessionItem) -> String {
    session
        .agent
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_lowercase()
        .replace([' ', '_'], "-")
}

fn wrap_index(index: isize, len: usize) -> usize {
    let len = len as isize;
    (((index % len) + len) % len) as usize
}

fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let lower = ch.to_ascii_lowercase() as u8;
            if lower.is_ascii_lowercase() {
                Some(vec![lower - b'a' + 1])
            } else {
                None
            }
        }
        KeyCode::Char(ch) => Some(ch.to_string().into_bytes()),
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_row_preserves_ansi_colors_and_modes() {
        let mut parser = vt100::Parser::new(2, 80, 0);
        parser.process(
            b"\x1b[31mred\x1b[0m \x1b[1;35mbold-purple\x1b[0m \x1b[38;2;80;180;255mtruecolor\x1b[0m",
        );

        let spans = render_terminal_row(parser.screen(), 0, 80);

        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "red" && span.style.fg == Some(Color::Indexed(1))));
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == "bold-purple"
                && span.style.fg == Some(Color::Indexed(5))
                && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == "truecolor"
                && span.style.fg == Some(Color::Rgb(80, 180, 255))
        }));
    }
}
