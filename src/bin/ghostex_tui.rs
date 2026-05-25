use std::collections::{BTreeMap, HashSet};
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
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde::Deserialize;

const POLL_INTERVAL: Duration = Duration::from_millis(16);
const SESSION_LIST_REFRESH: Duration = Duration::from_secs(5);
const TERMINAL_SCROLLBACK_LINES: usize = 10_000;
const MOUSE_SCROLL_LINES: usize = 3;
const GHOSTEX_TUI_TERM: &str = "xterm-256color";
const GHOSTEX_TUI_COLORTERM: &str = "truecolor";
const WORKING_COLOR: Color = Color::Rgb(248, 173, 7);
const ATTENTION_COLOR: Color = Color::Rgb(115, 231, 156);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionActivity {
    Attention,
    Working,
}

#[derive(Debug, Deserialize, Clone)]
struct SessionItem {
    #[serde(default)]
    activity: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default, rename = "projectId")]
    project_id: Option<String>,
    #[serde(default, rename = "groupId")]
    group_id: Option<String>,
    #[serde(default, rename = "projectName")]
    project_name: Option<String>,
    #[serde(default, rename = "projectPath")]
    project_path: Option<String>,
    #[serde(default, rename = "sessionId")]
    session_id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    title: String,
}

#[derive(Debug, Deserialize)]
struct SessionListResult {
    #[serde(default)]
    sessions: Vec<SessionItem>,
}

#[derive(Debug, Deserialize)]
struct CreateSessionResult {
    #[serde(default)]
    session: Option<CreatedSession>,
}

#[derive(Debug, Deserialize)]
struct CreatedSession {
    #[serde(default, rename = "ghostexId")]
    ghostex_id: Option<String>,
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ProjectGroup {
    project_id: Option<String>,
    group_id: Option<String>,
    name: String,
    sessions: Vec<SessionItem>,
}

#[derive(Debug, Clone)]
enum SwitchRow {
    Project(String),
    NewTerminal {
        project_id: Option<String>,
        group_id: Option<String>,
    },
    Session(SessionItem),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Attached,
    Switcher,
}

#[derive(Debug, Clone)]
enum SwitchAction {
    Attach(SessionItem),
    NewTerminal {
        project_id: Option<String>,
        group_id: Option<String>,
    },
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
        /*
        CDXC:GhostexTui 2026-05-25-17:18:
        Attached Ghostex panes must scroll like Herdr panes after `zmx attach`.
        Keep scrollback inside the TUI-owned terminal parser because the outer
        alternate screen cannot provide normal terminal scrollback for rendered
        pane contents.
        */
        let parser = vt100::Parser::new(
            area.height.max(1),
            area.width.max(1),
            TERMINAL_SCROLLBACK_LINES,
        );
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

    fn scroll_up(&mut self, lines: usize) {
        let next = self.parser.screen().scrollback().saturating_add(lines);
        self.parser.set_scrollback(next);
    }

    fn scroll_down(&mut self, lines: usize) {
        let next = self.parser.screen().scrollback().saturating_sub(lines);
        self.parser.set_scrollback(next);
    }

    fn scroll_reset(&mut self) {
        self.parser.set_scrollback(0);
    }

    fn handle_wheel(&mut self, mouse: MouseEvent, terminal_rect: Rect) {
        let column = mouse.column.saturating_sub(terminal_rect.x);
        let row = mouse.row.saturating_sub(terminal_rect.y);
        if self.parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None {
            if let Some(bytes) = encode_mouse_scroll(
                mouse.kind,
                column,
                row,
                mouse.modifiers,
                self.parser.screen().mouse_protocol_encoding(),
            ) {
                self.write_input(&bytes);
            }
            return;
        }
        if self.parser.screen().alternate_screen() {
            if let Some(bytes) =
                encode_alternate_scroll(mouse.kind, self.parser.screen().application_cursor())
            {
                self.write_input(&bytes);
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_up(MOUSE_SCROLL_LINES),
            MouseEventKind::ScrollDown => self.scroll_down(MOUSE_SCROLL_LINES),
            _ => {}
        }
    }

    fn write_input(&mut self, bytes: &[u8]) {
        self.scroll_reset();
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
    known_attention_session_ids: HashSet<String>,
    has_loaded_session_statuses: bool,
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
            known_attention_session_ids: HashSet::new(),
            has_loaded_session_statuses: false,
            status: String::new(),
        };
        app.refresh_sessions(false);
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

    fn refresh_sessions(&mut self, bell_on_new_attention: bool) {
        match fetch_sessions() {
            Ok(sessions) => {
                /*
                CDXC:GhostexTui 2026-05-25-16:22:
                The TUI polls Ghostex sidebar inventory every five seconds so
                switcher dots, attached-view counts, and bell notifications use
                the macOS app's activity source of truth instead of zmx state.
                */
                let next_attention_session_ids = attention_session_ids(&sessions);
                if bell_on_new_attention
                    && self.has_loaded_session_statuses
                    && next_attention_session_ids
                        .difference(&self.known_attention_session_ids)
                        .next()
                        .is_some()
                {
                    emit_terminal_bell();
                }
                self.known_attention_session_ids = next_attention_session_ids;
                self.has_loaded_session_statuses = true;
                let selected_session_id = self
                    .selected_session_at_row()
                    .map(|session| session.session_id.clone());
                let active_session_id = self
                    .active_session
                    .as_ref()
                    .map(|session| session.session_id.clone());
                self.groups = group_sessions(sessions);
                self.rows = switch_rows(&self.groups);
                if let Some(selected_session_id) = selected_session_id {
                    if let Some(row_index) = self.row_index_for_session_id(&selected_session_id) {
                        self.selected_row_index = row_index;
                    } else {
                        self.clamp_selected_row_to_selectable();
                    }
                } else {
                    self.clamp_selected_row_to_selectable();
                }
                self.sync_selected_session_index_from_row();
                if let Some(active_session_id) = active_session_id {
                    if let Some(session) = self.session_by_id(&active_session_id).cloned() {
                        self.active_session = Some(session);
                    }
                }
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
        if self.last_refresh.elapsed() >= SESSION_LIST_REFRESH {
            self.refresh_sessions(true);
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

    fn session_by_id(&self, session_id: &str) -> Option<&SessionItem> {
        self.rows.iter().find_map(|row| match row {
            SwitchRow::Session(session) if session.session_id == session_id => Some(session),
            _ => None,
        })
    }

    fn row_index_for_session_id(&self, session_id: &str) -> Option<usize> {
        self.rows.iter().position(
            |row| matches!(row, SwitchRow::Session(session) if session.session_id == session_id),
        )
    }

    fn activity_count(&self, activity: SessionActivity) -> usize {
        self.rows
            .iter()
            .filter(|row| match row {
                SwitchRow::Session(session) => session_activity(session) == Some(activity),
                SwitchRow::Project(_) | SwitchRow::NewTerminal { .. } => false,
            })
            .count()
    }

    fn selected_session_at_row(&self) -> Option<&SessionItem> {
        match self.rows.get(self.selected_row_index) {
            Some(SwitchRow::Session(session)) => Some(session),
            _ => None,
        }
    }

    fn selected_action(&self) -> Option<SwitchAction> {
        match self.rows.get(self.selected_row_index)?.clone() {
            SwitchRow::Session(session) => Some(SwitchAction::Attach(session)),
            SwitchRow::NewTerminal {
                project_id,
                group_id,
            } => Some(SwitchAction::NewTerminal {
                project_id,
                group_id,
            }),
            SwitchRow::Project(_) => None,
        }
    }

    fn selectable_row_indices(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(idx, row)| match row {
                SwitchRow::Project(_) => None,
                SwitchRow::NewTerminal { .. } | SwitchRow::Session(_) => Some(idx),
            })
            .collect()
    }

    fn sync_selected_session_index_from_row(&mut self) {
        self.selected_session_index = self
            .rows
            .iter()
            .take(self.selected_row_index)
            .filter(|row| matches!(row, SwitchRow::Session(_)))
            .count();
    }

    fn clamp_selected_row_to_selectable(&mut self) {
        let selectable_rows = self.selectable_row_indices();
        if selectable_rows.is_empty() {
            self.selected_row_index = 0;
            self.selected_session_index = 0;
            return;
        }
        if selectable_rows.contains(&self.selected_row_index) {
            return;
        }
        self.selected_row_index = selectable_rows
            .iter()
            .copied()
            .find(|row| *row >= self.selected_row_index)
            .unwrap_or_else(|| *selectable_rows.last().unwrap_or(&0));
        self.sync_selected_session_index_from_row();
    }

    fn select_delta(&mut self, delta: isize) {
        let selectable_rows = self.selectable_row_indices();
        if selectable_rows.is_empty() {
            return;
        }
        let current = selectable_rows
            .iter()
            .position(|row| *row == self.selected_row_index)
            .unwrap_or(0);
        let next = wrap_index(current as isize + delta, selectable_rows.len());
        self.selected_row_index = selectable_rows[next];
        self.sync_selected_session_index_from_row();
    }

    fn select_project_delta(&mut self, delta: isize) {
        /*
        CDXC:GhostexTui 2026-05-25-16:05:
        In the session switcher, left/right should move between projects by
        selecting each project's first session. Keep wrapping behavior so phone
        and keyboard users can cycle through project sections without landing
        on non-selectable headers.
        */
        let starts = self.project_first_session_rows();
        if starts.is_empty() {
            return;
        }
        let current_project = starts
            .iter()
            .enumerate()
            .rev()
            .find_map(|(idx, start)| {
                if *start <= self.selected_row_index {
                    Some(idx)
                } else {
                    None
                }
            })
            .unwrap_or(0);
        let next_project = wrap_index(current_project as isize + delta, starts.len());
        self.selected_row_index = starts[next_project];
        self.sync_selected_session_index_from_row();
    }

    fn project_first_session_rows(&self) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut in_project = false;
        let mut has_session_for_project = false;
        for (idx, row) in self.rows.iter().enumerate() {
            match row {
                SwitchRow::Project(_) => {
                    in_project = true;
                    has_session_for_project = false;
                }
                SwitchRow::Session(_) if in_project && !has_session_for_project => {
                    starts.push(idx);
                    has_session_for_project = true;
                }
                SwitchRow::NewTerminal { .. } | SwitchRow::Session(_) => {}
            }
        }
        starts
    }

    fn select_row_at_document_y(&mut self, doc_y: usize) -> Option<SwitchAction> {
        let row = self.rows.get(doc_y)?.clone();
        if matches!(row, SwitchRow::Project(_)) {
            return None;
        }
        self.selected_row_index = doc_y;
        self.sync_selected_session_index_from_row();
        self.selected_action()
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
                Event::Mouse(mouse) => {
                    if handle_mouse(&mut app, mouse, size, terminal_rect) {
                        break;
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
    Ok(())
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(area);
    render_header(frame, app, chunks[0]);
    match app.mode {
        Mode::Attached => render_terminal(frame, app, chunks[1]),
        Mode::Switcher => render_switcher(frame, app, chunks[1]),
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    /*
    CDXC:GhostexTui 2026-05-25-17:24:
    The attached TUI needs visible chrome separation from the terminal pane.
    Render the top bar on a full-width background with a bottom rule so phone
    and desktop users can distinguish Ghostex controls from session output.

    CDXC:GhostexTui 2026-05-25-17:38:
    The header title may wrap onto the second content row while activity counts
    stay below it on the right. Keep the switch affordance two rows tall and
    label it as "switch session" with one word per row.

    CDXC:GhostexTui 2026-05-25-17:48:
    When the user is already on the switcher, the top-right control should
    become the exit affordance and read "Quit GTX TUI" instead of offering to
    switch sessions again.
    */
    let header_style = Style::default().bg(Color::Rgb(24, 24, 37));
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new("").style(header_style), area);
    let switch_width = 12u16.min(area.width);
    let status_width = area.width.saturating_sub(switch_width);
    let status = Rect::new(area.x, area.y, status_width, area.height);
    let switch = switch_button_rect(area);
    let title = app
        .active_session
        .as_ref()
        .map(|session| session.title.as_str())
        .unwrap_or("No session");
    let count_width = if app.mode == Mode::Attached {
        activity_count_width(
            app.activity_count(SessionActivity::Working),
            app.activity_count(SessionActivity::Attention),
        )
        .min(status_width)
    } else {
        0
    };
    let title_width = status_width
        .saturating_sub(count_width)
        .max(status_width.min(1));
    let title_spans = vec![
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
            header_style.fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ];
    if app.mode == Mode::Attached {
        /*
        CDXC:GhostexTui 2026-05-25-17:23:
        Attached-view activity totals should be compact dot counters without
        the literal words "working" or "attention"; the dot colors carry the
        same meaning as the macOS sidebar indicators.
        */
        let counts = activity_count_spans(
            app.activity_count(SessionActivity::Working),
            app.activity_count(SessionActivity::Attention),
        );
        frame.render_widget(
            Paragraph::new(Line::from(counts))
                .style(header_style)
                .alignment(Alignment::Right),
            Rect::new(
                status.x + status_width.saturating_sub(count_width),
                status.y + 1,
                count_width,
                1,
            ),
        );
    }
    frame.render_widget(
        Paragraph::new(Line::from(title_spans))
            .style(header_style)
            .wrap(Wrap { trim: false }),
        Rect::new(
            status.x,
            status.y,
            title_width,
            area.height.saturating_sub(1),
        ),
    );
    frame.render_widget(
        Paragraph::new(match app.mode {
            Mode::Attached => "switch\nsession",
            Mode::Switcher => "Quit GTX\nTUI",
        })
        .style(
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(49, 50, 68))
                .add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::LEFT)),
        switch,
    );
    if area.height > 0 {
        let border_y = area.y + area.height.saturating_sub(1);
        frame.render_widget(
            Paragraph::new("─".repeat(area.width as usize)).style(
                Style::default()
                    .fg(Color::Rgb(69, 71, 90))
                    .bg(Color::Rgb(24, 24, 37)),
            ),
            Rect::new(area.x, border_y, area.width, 1),
        );
    }
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

fn activity_dot_span(session: &SessionItem, bg: Color) -> Span<'static> {
    match session_activity(session) {
        Some(activity) => Span::styled(" ●", Style::default().fg(activity_color(activity)).bg(bg)),
        None => Span::styled("  ", Style::default().bg(bg)),
    }
}

fn activity_count_spans(working_count: usize, attention_count: usize) -> Vec<Span<'static>> {
    vec![
        Span::styled("●", Style::default().fg(WORKING_COLOR)),
        Span::styled(
            format!(" {working_count}"),
            Style::default().fg(Color::White),
        ),
        Span::raw("  "),
        Span::styled("●", Style::default().fg(ATTENTION_COLOR)),
        Span::styled(
            format!(" {attention_count}"),
            Style::default().fg(Color::White),
        ),
    ]
}

fn activity_count_width(working_count: usize, attention_count: usize) -> u16 {
    format!("● {working_count}  ● {attention_count}")
        .chars()
        .count() as u16
}

fn activity_color(activity: SessionActivity) -> Color {
    match activity {
        SessionActivity::Attention => ATTENTION_COLOR,
        SessionActivity::Working => WORKING_COLOR,
    }
}

fn session_activity(session: &SessionItem) -> Option<SessionActivity> {
    let value = session
        .activity
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(session.status.as_str())
        .trim()
        .to_lowercase();
    match value.as_str() {
        "attention" => Some(SessionActivity::Attention),
        "working" => Some(SessionActivity::Working),
        _ => None,
    }
}

fn attention_session_ids(sessions: &[SessionItem]) -> HashSet<String> {
    sessions
        .iter()
        .filter(|session| session_activity(session) == Some(SessionActivity::Attention))
        .map(|session| session.session_id.clone())
        .collect()
}

fn emit_terminal_bell() {
    let _ = io::stdout().write_all(b"\x07");
    let _ = io::stdout().flush();
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
            SwitchRow::NewTerminal { .. } => {
                let selected = idx == app.selected_row_index;
                let bg = if selected {
                    Color::Rgb(49, 50, 68)
                } else {
                    Color::Reset
                };
                /*
                CDXC:GhostexTui 2026-05-25-17:20:
                Each switcher project should expose a create-terminal action
                before its sessions. It creates a terminal in that project/group
                through the existing Ghostex CLI create-session bridge so the
                macOS app remains the owner of project placement and zmx setup.

                CDXC:GhostexTui 2026-05-25-17:48:
                The create-terminal row should read "Create new terminal" in a
                lighter color and without a leading plus, so it feels like a
                quiet project action rather than another agent/session row.
                */
                ListItem::new(Line::from(Span::styled(
                    "  Create new terminal",
                    Style::default()
                        .fg(Color::Rgb(205, 214, 244))
                        .bg(bg)
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                )))
                .style(Style::default().bg(bg))
            }
            SwitchRow::Session(session) => {
                let selected = idx == app.selected_row_index;
                let bg = if selected {
                    Color::Rgb(49, 50, 68)
                } else {
                    Color::Reset
                };
                let mut spans = vec![
                    activity_dot_span(session, bg),
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
                ];
                ListItem::new(Line::from(std::mem::take(&mut spans))).style(Style::default().bg(bg))
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
            KeyCode::Left => app.select_project_delta(-1),
            KeyCode::Right => app.select_project_delta(1),
            KeyCode::PageUp => app.select_delta(-5),
            KeyCode::PageDown => app.select_delta(5),
            KeyCode::Enter | KeyCode::Char(' ') => handle_switch_action(app, terminal_rect),
            _ => {}
        },
        Mode::Attached => {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('s'))
            {
                app.mode = Mode::Switcher;
                app.refresh_sessions(false);
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

fn handle_mouse(app: &mut App, mouse: MouseEvent, full: Rect, terminal_rect: Rect) -> bool {
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
                app.refresh_sessions(false);
            }
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) && rect_contains(terminal_rect, mouse.column, mouse.row)
            {
                if let Some(pty) = app.pty.as_mut() {
                    pty.handle_wheel(mouse, terminal_rect);
                }
            }
        }
        Mode::Switcher => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if rect_contains(
                    switch_button_rect(header_area(full)),
                    mouse.column,
                    mouse.row,
                ) =>
            {
                return true;
            }
            MouseEventKind::ScrollUp => app.select_delta(-(MOUSE_SCROLL_LINES as isize)),
            MouseEventKind::ScrollDown => app.select_delta(MOUSE_SCROLL_LINES as isize),
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.row < terminal_rect.y {
                    return false;
                }
                let doc_y = app
                    .switch_scroll
                    .saturating_add(mouse.row.saturating_sub(terminal_rect.y) as usize);
                if app.select_row_at_document_y(doc_y).is_some() {
                    handle_switch_action(app, terminal_rect);
                }
            }
            _ => {}
        },
    }
    false
}

fn handle_switch_action(app: &mut App, terminal_rect: Rect) {
    match app.selected_action() {
        Some(SwitchAction::Attach(session)) => app.attach(session, terminal_rect),
        Some(SwitchAction::NewTerminal {
            project_id,
            group_id,
        }) => match create_terminal(project_id.as_deref(), group_id.as_deref()) {
            Ok(created) => {
                app.status.clear();
                app.refresh_sessions(false);
                if let Some(session_id) = created
                    .session
                    .and_then(|session| session.ghostex_id.or(session.session_id))
                {
                    if let Some(row_index) = app.row_index_for_session_id(&session_id) {
                        app.selected_row_index = row_index;
                        app.sync_selected_session_index_from_row();
                    }
                }
            }
            Err(err) => {
                app.status = format!("Could not create terminal: {err}");
            }
        },
        None => {}
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

fn create_terminal(
    project_id: Option<&str>,
    group_id: Option<&str>,
) -> io::Result<CreateSessionResult> {
    let mut command = format!("{} create-session", ghostex_cli_command());
    if let Some(project_id) = project_id.filter(|value| !value.trim().is_empty()) {
        command.push_str(" --project-id ");
        command.push_str(&shell_quote(project_id));
    }
    if let Some(group_id) = group_id.filter(|value| !value.trim().is_empty()) {
        command.push_str(" --group-id ");
        command.push_str(&shell_quote(group_id));
    }
    let output = Command::new("/bin/zsh").arg("-lc").arg(command).output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
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
                project_id: session.project_id.clone(),
                group_id: session.group_id.clone(),
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
        rows.push(SwitchRow::NewTerminal {
            project_id: group.project_id.clone(),
            group_id: group.group_id.clone(),
        });
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
    Rect::new(full.x, full.y, full.width, full.height.min(3))
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
        header.height.min(2),
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

fn encode_mouse_scroll(
    kind: MouseEventKind,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let button = match kind {
        MouseEventKind::ScrollUp => 64u16,
        MouseEventKind::ScrollDown => 65u16,
        _ => return None,
    };
    encode_mouse_cb(button, false, column, row, modifiers, encoding)
}

fn encode_mouse_cb(
    base_button: u16,
    release: bool,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let mut cb = match (encoding, release) {
        (vt100::MouseProtocolEncoding::Sgr, true) => base_button,
        (_, true) => 3,
        (_, false) => base_button,
    };
    if modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    let column = column as u32 + 1;
    let row = row as u32 + 1;
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => Some(
            format!(
                "\x1b[<{cb};{column};{row}{}",
                if release { 'm' } else { 'M' }
            )
            .into_bytes(),
        ),
        vt100::MouseProtocolEncoding::Default => {
            let cb = u8::try_from(cb + 32).ok()?;
            let column = u8::try_from(column + 32).ok()?;
            let row = u8::try_from(row + 32).ok()?;
            Some(vec![0x1b, b'[', b'M', cb, column, row])
        }
        vt100::MouseProtocolEncoding::Utf8 => {
            let mut bytes = Vec::with_capacity(16);
            bytes.extend_from_slice(b"\x1b[M");
            push_mouse_codepoint(&mut bytes, cb as u32 + 32)?;
            push_mouse_codepoint(&mut bytes, column + 32)?;
            push_mouse_codepoint(&mut bytes, row + 32)?;
            Some(bytes)
        }
    }
}

fn push_mouse_codepoint(bytes: &mut Vec<u8>, value: u32) -> Option<()> {
    let ch = char::from_u32(value)?;
    let mut buf = [0u8; 4];
    bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    Some(())
}

fn encode_alternate_scroll(kind: MouseEventKind, application_cursor: bool) -> Option<Vec<u8>> {
    match (kind, application_cursor) {
        (MouseEventKind::ScrollUp, true) => Some(b"\x1bOA".to_vec()),
        (MouseEventKind::ScrollDown, true) => Some(b"\x1bOB".to_vec()),
        (MouseEventKind::ScrollUp, false) => Some(b"\x1b[A".to_vec()),
        (MouseEventKind::ScrollDown, false) => Some(b"\x1b[B".to_vec()),
        _ => None,
    }
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

    fn test_session(project_id: &str, title: &str) -> SessionItem {
        SessionItem {
            activity: None,
            agent: Some("codex".to_string()),
            project_id: Some(project_id.to_string()),
            group_id: Some(format!("{project_id}-group")),
            project_name: Some(project_id.to_string()),
            project_path: Some(format!("/{project_id}")),
            session_id: format!("{project_id}-{title}"),
            status: "idle".to_string(),
            title: title.to_string(),
        }
    }

    fn test_app(groups: Vec<ProjectGroup>) -> App {
        let rows = switch_rows(&groups);
        App {
            groups,
            rows,
            selected_session_index: 0,
            selected_row_index: 1,
            active_session: None,
            pty: None,
            mode: Mode::Switcher,
            switch_scroll: 0,
            last_refresh: Instant::now(),
            known_attention_session_ids: HashSet::new(),
            has_loaded_session_statuses: true,
            status: String::new(),
        }
    }

    #[test]
    fn switcher_left_right_jump_to_project_first_sessions() {
        let mut app = test_app(vec![
            ProjectGroup {
                project_id: Some("alpha".to_string()),
                group_id: Some("alpha-group".to_string()),
                name: "alpha".to_string(),
                sessions: vec![test_session("alpha", "one"), test_session("alpha", "two")],
            },
            ProjectGroup {
                project_id: Some("beta".to_string()),
                group_id: Some("beta-group".to_string()),
                name: "beta".to_string(),
                sessions: vec![test_session("beta", "one"), test_session("beta", "two")],
            },
            ProjectGroup {
                project_id: Some("gamma".to_string()),
                group_id: Some("gamma-group".to_string()),
                name: "gamma".to_string(),
                sessions: vec![test_session("gamma", "one")],
            },
        ]);

        app.selected_row_index = 2;
        app.sync_selected_session_index_from_row();
        app.select_delta(1);
        assert_eq!(app.selected_row_index, 3);

        app.select_project_delta(1);
        assert_eq!(app.selected_row_index, 6);

        app.select_project_delta(1);
        assert_eq!(app.selected_row_index, 10);

        app.select_project_delta(1);
        assert_eq!(app.selected_row_index, 2);

        app.select_project_delta(-1);
        assert_eq!(app.selected_row_index, 10);
    }

    #[test]
    fn terminal_row_preserves_ansi_colors_and_modes() {
        let mut parser = vt100::Parser::new(2, 80, 0);
        parser.process(
            b"\x1b[31mred\x1b[0m \x1b[1;35mbold-purple\x1b[0m \x1b[38;2;80;180;255mtruecolor\x1b[0m",
        );

        let spans = render_terminal_row(parser.screen(), 0, 80);

        assert!(
            spans
                .iter()
                .any(|span| span.content.as_ref() == "red"
                    && span.style.fg == Some(Color::Indexed(1)))
        );
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == "bold-purple"
                && span.style.fg == Some(Color::Indexed(5))
                && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == "truecolor" && span.style.fg == Some(Color::Rgb(80, 180, 255))
        }));
    }

    #[test]
    fn session_activity_prefers_sidebar_activity_over_lifecycle_status() {
        let mut session = test_session("alpha", "one");
        session.status = "done".to_string();
        session.activity = Some("attention".to_string());

        assert_eq!(session_activity(&session), Some(SessionActivity::Attention));

        session.activity = None;
        session.status = "working".to_string();
        assert_eq!(session_activity(&session), Some(SessionActivity::Working));
    }

    #[test]
    fn activity_counts_follow_refreshed_rows() {
        let app = test_app(vec![ProjectGroup {
            project_id: Some("alpha".to_string()),
            group_id: Some("alpha-group".to_string()),
            name: "alpha".to_string(),
            sessions: vec![
                SessionItem {
                    activity: Some("working".to_string()),
                    ..test_session("alpha", "working")
                },
                SessionItem {
                    activity: Some("attention".to_string()),
                    ..test_session("alpha", "attention")
                },
                test_session("alpha", "idle"),
            ],
        }]);

        assert_eq!(app.activity_count(SessionActivity::Working), 1);
        assert_eq!(app.activity_count(SessionActivity::Attention), 1);
    }
}
