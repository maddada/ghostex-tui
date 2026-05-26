use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use herdr::{config, events, layout, pane, terminal, terminal_theme};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde::Deserialize;
use tokio::sync::{mpsc as tokio_mpsc, Notify};

const POLL_INTERVAL: Duration = Duration::from_millis(16);
const SESSION_LIST_REFRESH: Duration = Duration::from_secs(5);
const TERMINAL_SCROLLBACK_BYTES: usize = config::DEFAULT_SCROLLBACK_LIMIT_BYTES;
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
    #[serde(default, rename = "attachCommand")]
    attach_command: Option<String>,
    #[serde(default, rename = "isFavorite")]
    is_favorite: Option<bool>,
    #[serde(default, rename = "projectName")]
    project_name: Option<String>,
    #[serde(default, rename = "projectPath")]
    project_path: Option<String>,
    #[serde(default, rename = "sessionId")]
    session_id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    resume_command: Option<String>,
    #[serde(default, rename = "resumeFallbackCommand")]
    resume_fallback_command: Option<String>,
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
    path: Option<String>,
    sessions: Vec<SessionItem>,
}

#[derive(Debug, Clone)]
enum SwitchRow {
    Project(ProjectHeader),
    NewTerminal {
        project_id: Option<String>,
        group_id: Option<String>,
    },
    Session(SessionItem),
}

#[derive(Debug, Clone)]
struct ProjectHeader {
    project_id: Option<String>,
    group_id: Option<String>,
    name: String,
    path: Option<String>,
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

#[derive(Debug, Clone)]
struct ContextMenu {
    title: String,
    actions: Vec<ContextAction>,
    selected_index: usize,
}

#[derive(Debug, Clone)]
struct ContextAction {
    danger: bool,
    label: String,
    kind: ContextActionKind,
}

#[derive(Debug, Clone)]
enum ContextActionKind {
    CopyAttachCommand(SessionItem),
    CopyProjectPath(ProjectHeader),
    CopyResumeCommand(SessionItem),
    CreateTerminal {
        project_id: Option<String>,
        group_id: Option<String>,
    },
    FocusGroup(ProjectHeader),
    OpenProjectInFinder(ProjectHeader),
    ProjectCloseSessions(ProjectHeader),
    ProjectFullReload(ProjectHeader),
    ProjectMove {
        project_id: Option<String>,
        direction: &'static str,
    },
    ProjectSleep {
        header: ProjectHeader,
        sleeping: bool,
    },
    RenameSession(SessionItem),
    SessionAttach(SessionItem),
    SessionClose(SessionItem),
    SessionFavorite {
        session: SessionItem,
        favorite: bool,
    },
    SessionFork(SessionItem),
    SessionFullReload(SessionItem),
    SessionSleep {
        session: SessionItem,
        sleeping: bool,
    },
}

#[derive(Debug, Clone)]
struct InputPrompt {
    title: String,
    value: String,
    action: InputPromptAction,
}

#[derive(Debug, Clone)]
enum InputPromptAction {
    RenameSession(SessionItem),
}

struct PtySession {
    pane_id: layout::PaneId,
    runtime: terminal::TerminalRuntime,
    events_rx: tokio_mpsc::Receiver<events::AppEvent>,
    render_dirty: Arc<AtomicBool>,
    _render_notify: Arc<Notify>,
}

impl PtySession {
    fn spawn(session: &SessionItem, area: Rect) -> io::Result<Self> {
        let shell_command = format!(
            "{} attach --session-id {}",
            ghostex_cli_command(),
            shell_quote(&session.session_id)
        );
        let pane_id = layout::PaneId::alloc();
        let (events_tx, events_rx) = tokio_mpsc::channel(32);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));
        let cwd = session
            .project_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);
        /*
        CDXC:GhostexTui 2026-05-26-10:41:
        Attached Ghostex panes must use Herdr's Ghostty-backed TerminalRuntime,
        not the earlier vt100 wrapper. The user expects mouse wheel scrollback
        to behave exactly like Herdr after `zmx attach`, while alternate-screen
        apps still receive mouse reports or xterm alternate-scroll when they
        explicitly enable those terminal modes.

        CDXC:GhostexTui 2026-05-25-15:50:
        The attached session PTY is rendered by Ghostex TUI, not by the outer
        shell that launched `gtx`. Force a real terminal identity so Codex CLI,
        Starship, and other terminal-aware tools do not inherit TERM=dumb from
        desktop launchers or non-terminal hosts.

        CDXC:GhostexTui 2026-05-26-11:29:
        The CLI attach path now uses full zmx replay for all live zmx clients,
        so the TUI does not need a special attach marker to receive scrollback.
        */
        let runtime = terminal::TerminalRuntime::spawn_shell_command(
            pane_id,
            area.height.max(1),
            area.width.max(1),
            cwd,
            &shell_command,
            &[
                ("TERM".to_string(), GHOSTEX_TUI_TERM.to_string()),
                ("COLORTERM".to_string(), GHOSTEX_TUI_COLORTERM.to_string()),
                ("TERM_PROGRAM".to_string(), "ghostex-tui".to_string()),
            ],
            TERMINAL_SCROLLBACK_BYTES,
            terminal_theme::TerminalTheme::default(),
            events_tx,
            render_notify.clone(),
            render_dirty.clone(),
        )?;
        Ok(Self {
            pane_id,
            runtime,
            events_rx,
            render_dirty,
            _render_notify: render_notify,
        })
    }

    fn resize(&self, area: Rect) {
        self.runtime
            .resize(area.height.max(1), area.width.max(1), 0, 0);
    }

    fn drain_output(&mut self) {
        self.render_dirty.swap(false, Ordering::AcqRel);
        while let Ok(event) = self.events_rx.try_recv() {
            if let events::AppEvent::PaneDied { pane_id } = event {
                if pane_id == self.pane_id {
                    break;
                }
            }
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, terminal_rect: Rect) {
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            self.handle_wheel(mouse, terminal_rect);
            return;
        }
        let column = mouse.column.saturating_sub(terminal_rect.x);
        let row = mouse.row.saturating_sub(terminal_rect.y);
        if let Some(bytes) =
            self.runtime
                .encode_mouse_button(mouse.kind, column, row, mouse.modifiers)
        {
            self.runtime.scroll_reset();
            self.write_input(bytes);
        }
    }

    fn handle_wheel(&mut self, mouse: MouseEvent, terminal_rect: Rect) {
        match self.runtime.wheel_routing() {
            Some(pane::WheelRouting::HostScroll) | None => match mouse.kind {
                MouseEventKind::ScrollUp => self.runtime.scroll_up(MOUSE_SCROLL_LINES),
                MouseEventKind::ScrollDown => self.runtime.scroll_down(MOUSE_SCROLL_LINES),
                _ => {}
            },
            Some(pane::WheelRouting::MouseReport) => {
                self.runtime.scroll_reset();
                let column = mouse.column.saturating_sub(terminal_rect.x);
                let row = mouse.row.saturating_sub(terminal_rect.y);
                if let Some(bytes) =
                    self.runtime
                        .encode_mouse_wheel(mouse.kind, column, row, mouse.modifiers)
                {
                    self.write_input(bytes);
                }
            }
            Some(pane::WheelRouting::AlternateScroll) => {
                self.runtime.scroll_reset();
                if let Some(bytes) = self.runtime.encode_alternate_scroll(mouse.kind) {
                    self.write_input(bytes);
                }
            }
        }
    }

    fn write_key(&mut self, key: KeyEvent) {
        let bytes = self.runtime.encode_terminal_key(key.into());
        if !bytes.is_empty() {
            self.runtime.scroll_reset();
            self.write_input(bytes);
        }
    }

    fn write_input(&mut self, bytes: Vec<u8>) {
        let _ = self.runtime.try_send_bytes(Bytes::from(bytes));
    }
}

struct App {
    groups: Vec<ProjectGroup>,
    rows: Vec<SwitchRow>,
    selected_session_index: usize,
    selected_row_index: usize,
    active_session: Option<SessionItem>,
    context_menu: Option<ContextMenu>,
    input_prompt: Option<InputPrompt>,
    show_hotkeys: bool,
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
            context_menu: None,
            input_prompt: None,
            show_hotkeys: false,
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
        /*
        CDXC:GhostexTui 2026-05-26-13:03:
        Attaching to an attention session from GTX TUI means the user has seen
        that shared attention event. Acknowledge through the Ghostex CLI bridge
        so the desktop sidebar and any other TUI clients clear the same event.
        */
        if session_activity(&session) == Some(SessionActivity::Attention) {
            let _ = acknowledge_session_attention(&session);
            self.known_attention_session_ids.remove(&session.session_id);
        }
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
                SwitchRow::Project(_) | SwitchRow::NewTerminal { .. } | SwitchRow::Session(_) => {
                    Some(idx)
                }
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
        self.rows.get(doc_y)?;
        self.selected_row_index = doc_y;
        self.sync_selected_session_index_from_row();
        self.selected_action()
    }

    fn open_context_menu(&mut self) {
        /*
        CDXC:GhostexTui 2026-05-25-18:08:
        Ctrl+K opens one keyboard-driven context menu for both session rows and
        project headers. The menu mirrors the macOS sidebar right-click actions
        that are executable from the CLI/TUI context, with project group actions
        applied to the same grouped sessions visible under that header.
        */
        let Some(row) = self.rows.get(self.selected_row_index).cloned() else {
            return;
        };
        self.context_menu = match row {
            SwitchRow::Project(header) => Some(ContextMenu {
                title: header.name.clone(),
                actions: project_context_actions(&header, self.project_sessions(&header)),
                selected_index: 0,
            }),
            SwitchRow::Session(session) => Some(ContextMenu {
                title: session.title.clone(),
                actions: session_context_actions(&session),
                selected_index: 0,
            }),
            SwitchRow::NewTerminal {
                project_id,
                group_id,
            } => Some(ContextMenu {
                title: "Create new terminal".to_string(),
                actions: vec![ContextAction {
                    danger: false,
                    label: "Create new terminal".to_string(),
                    kind: ContextActionKind::CreateTerminal {
                        project_id,
                        group_id,
                    },
                }],
                selected_index: 0,
            }),
        };
    }

    fn project_sessions(&self, header: &ProjectHeader) -> Vec<SessionItem> {
        self.groups
            .iter()
            .find(|group| {
                group.group_id == header.group_id && group.project_id == header.project_id
            })
            .map(|group| group.sessions.clone())
            .unwrap_or_default()
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

#[tokio::main]
async fn main() -> io::Result<()> {
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
    /*
    CDXC:GhostexTui 2026-05-25-18:17:
    The Ghostex TUI control bar belongs at the bottom of the screen, leaving
    terminal output and the switcher above it. Keep all hit-testing on the
    shared header_area/terminal_area helpers so mouse behavior follows the
    rendered layout.
    */
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(area);
    match app.mode {
        Mode::Attached => render_terminal(frame, app, chunks[0]),
        Mode::Switcher => render_switcher(frame, app, chunks[0]),
    }
    render_header(frame, app, chunks[1]);
    if let Some(menu) = app.context_menu.as_ref() {
        render_context_menu(frame, menu, area);
    }
    if let Some(prompt) = app.input_prompt.as_ref() {
        render_input_prompt(frame, prompt, area);
    }
    if app.show_hotkeys {
        render_hotkeys_overlay(frame, area);
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    /*
    CDXC:GhostexTui 2026-05-25-17:24:
    The attached TUI needs visible chrome separation from the terminal pane.
    Render the top bar on a full-width background with a boundary rule so phone
    and desktop users can distinguish Ghostex controls from session output.

    CDXC:GhostexTui 2026-05-25-17:38:
    The header title may wrap onto the second content row while activity counts
    stay below it on the right. Keep the switch affordance two rows tall and
    label it as "switch session" with one word per row.

    CDXC:GhostexTui 2026-05-25-17:48:
    When the user is already on the switcher, the top-right control should
    become the exit affordance and read "Quit GTX TUI" instead of offering to
    switch sessions again.

    CDXC:GhostexTui 2026-05-25-17:58:
    The switcher quit label should keep "Quit" on the first row and "GTX TUI"
    on the second row, while attached activity counters reserve one trailing
    space before the switch button so the attention count does not touch it.

    CDXC:GhostexTui 2026-05-25-18:37:
    The TUI title bar sits at the bottom of the screen, so its separator must
    render on the top edge instead of below the bar to divide session output
    from Ghostex controls.

    CDXC:GhostexTui 2026-05-26-09:10:
    In switcher mode, replace the left title area with a Hotkeys button that
    opens the shortcuts overlay. The switcher already shows session context,
    so repeating the Ghostex/title label is less useful than discoverable TUI
    controls.

    CDXC:GhostexTui 2026-05-26-03:39:
    Attached-view working/attention totals belong on the second content line,
    right-aligned with exactly one column of margin before the switch control.
    */
    let header_style = Style::default().bg(Color::Rgb(24, 24, 37));
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new("").style(header_style), area);
    let switch_width = 12u16.min(area.width);
    let status_width = area.width.saturating_sub(switch_width);
    let status = Rect::new(area.x, area.y, status_width, area.height);
    let switch = switch_button_rect(area);
    if app.mode == Mode::Switcher {
        frame.render_widget(
            Paragraph::new("Hotkeys")
                .style(
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Rgb(49, 50, 68))
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::RIGHT)),
            hotkeys_button_rect(area),
        );
    }
    let title = app
        .active_session
        .as_ref()
        .map(|session| session.title.as_str())
        .unwrap_or("No session");
    let count_right_margin = 1u16;
    let count_width = if app.mode == Mode::Attached {
        activity_count_width(
            app.activity_count(SessionActivity::Working),
            app.activity_count(SessionActivity::Attention),
        )
        .min(status_width.saturating_sub(count_right_margin))
    } else {
        0
    };
    let title_width = status_width
        .saturating_sub(count_width.saturating_add(count_right_margin))
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
                status.x
                    + status_width.saturating_sub(count_width.saturating_add(count_right_margin)),
                status.y + 2,
                count_width,
                1,
            ),
        );
    }
    if app.mode == Mode::Attached {
        frame.render_widget(
            Paragraph::new(Line::from(title_spans))
                .style(header_style)
                .wrap(Wrap { trim: false }),
            Rect::new(
                status.x,
                status.y + 1,
                title_width,
                area.height.saturating_sub(1),
            ),
        );
    }
    frame.render_widget(
        Paragraph::new(match app.mode {
            Mode::Attached => "switch\nsession",
            Mode::Switcher => "Quit\nGTX TUI",
        })
        .style(
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(49, 50, 68))
                .add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::LEFT)),
        Rect::new(switch.x, switch.y + 1, switch.width, switch.height),
    );
    if area.height > 0 {
        frame.render_widget(
            Paragraph::new("─".repeat(area.width as usize)).style(
                Style::default()
                    .fg(Color::Rgb(69, 71, 90))
                    .bg(Color::Rgb(24, 24, 37)),
            ),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
}

fn render_terminal(frame: &mut Frame, app: &mut App, area: Rect) {
    if let Some(pty) = app.pty.as_ref() {
        pty.runtime.render(frame, area, true);
    } else {
        frame.render_widget(
            Paragraph::new(app.status.as_str()).style(Style::default().fg(Color::Red)),
            area,
        );
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
            SwitchRow::Project(project) => {
                let selected = idx == app.selected_row_index;
                let bg = if selected {
                    Color::Rgb(49, 50, 68)
                } else {
                    Color::Reset
                };
                ListItem::new(Line::from(Span::styled(
                    project.name.clone(),
                    Style::default()
                        .fg(Color::Rgb(137, 180, 250))
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                )))
                .style(Style::default().bg(bg))
            }
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

                CDXC:GhostexTui 2026-05-25-17:58:
                Indent the create-terminal action two more columns than the
                first implementation so it reads as a nested project command.
                */
                ListItem::new(Line::from(Span::styled(
                    "    Create new terminal",
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

fn render_context_menu(frame: &mut Frame, menu: &ContextMenu, full: Rect) {
    let width = full.width.min(44).max(24);
    let height = (menu.actions.len() as u16 + 2).min(full.height.saturating_sub(2).max(3));
    let area = centered_rect(width, height, full);
    frame.render_widget(Clear, area);
    let visible_actions = menu
        .actions
        .iter()
        .enumerate()
        .map(|(idx, action)| {
            let selected = idx == menu.selected_index;
            let bg = if selected {
                Color::Rgb(49, 50, 68)
            } else {
                Color::Rgb(24, 24, 37)
            };
            let fg = if action.danger {
                Color::Rgb(255, 121, 121)
            } else {
                Color::White
            };
            ListItem::new(Line::from(Span::styled(
                action.label.clone(),
                Style::default().fg(fg).bg(bg).add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            )))
            .style(Style::default().bg(bg))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(Some(menu.selected_index));
    frame.render_stateful_widget(
        List::new(visible_actions)
            .block(
                Block::default()
                    .title(format!(" {} ", menu.title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Rgb(137, 180, 250)))
                    .style(Style::default().bg(Color::Rgb(24, 24, 37))),
            )
            .highlight_symbol(" "),
        area,
        &mut state,
    );
}

fn render_input_prompt(frame: &mut Frame, prompt: &InputPrompt, full: Rect) {
    let width = full.width.min(60).max(28);
    let area = centered_rect(width, 5, full);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(format!("{}\n\n{}", prompt.title, prompt.value))
            .style(Style::default().fg(Color::White).bg(Color::Rgb(24, 24, 37)))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Rgb(137, 180, 250))),
            ),
        area,
    );
}

fn render_hotkeys_overlay(frame: &mut Frame, full: Rect) {
    /*
    CDXC:GhostexTui 2026-05-26-09:10:
    The switcher hotkeys button should open an in-TUI reference for every
    current keyboard shortcut. Keep this overlay read-only and dismissible so
    it does not introduce a settings surface or change terminal input behavior.
    */
    let width = full.width.min(54).max(32);
    let area = centered_rect(width, 14, full);
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(vec![
            Span::styled("Ctrl+Q", Style::default().fg(Color::Rgb(137, 180, 250))),
            Span::raw("  Quit GTX TUI"),
        ]),
        Line::from(vec![
            Span::styled("Ctrl+S", Style::default().fg(Color::Rgb(137, 180, 250))),
            Span::raw("  Open switcher from attached session"),
        ]),
        Line::from(vec![
            Span::styled("Ctrl+K", Style::default().fg(Color::Rgb(137, 180, 250))),
            Span::raw("  Open context menu"),
        ]),
        Line::from(vec![
            Span::styled("Esc", Style::default().fg(Color::Rgb(137, 180, 250))),
            Span::raw("     Close overlay/menu, or return to session"),
        ]),
        Line::from(vec![
            Span::styled("Up/Down", Style::default().fg(Color::Rgb(137, 180, 250))),
            Span::raw(" Move selection"),
        ]),
        Line::from(vec![
            Span::styled("Left/Right", Style::default().fg(Color::Rgb(137, 180, 250))),
            Span::raw(" Jump projects"),
        ]),
        Line::from(vec![
            Span::styled("PgUp/PgDn", Style::default().fg(Color::Rgb(137, 180, 250))),
            Span::raw(" Jump 5 rows"),
        ]),
        Line::from(vec![
            Span::styled(
                "Enter/Space",
                Style::default().fg(Color::Rgb(137, 180, 250)),
            ),
            Span::raw(" Attach, create, or confirm"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Click anywhere or press Esc to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(Color::White).bg(Color::Rgb(24, 24, 37)))
            .block(
                Block::default()
                    .title(" Hotkeys ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Rgb(137, 180, 250))),
            ),
        area,
    );
}

fn handle_key(app: &mut App, key: KeyEvent, terminal_rect: Rect) -> bool {
    if app.show_hotkeys {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
            return true;
        }
        app.show_hotkeys = false;
        return false;
    }
    if app.input_prompt.is_some() {
        return handle_input_prompt_key(app, key);
    }
    if app.context_menu.is_some() {
        return handle_context_menu_key(app, key, terminal_rect);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
        return true;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('k')) {
        if app.mode == Mode::Attached {
            if let Some(session) = app.active_session.clone() {
                app.context_menu = Some(ContextMenu {
                    title: session.title.clone(),
                    actions: session_context_actions(&session),
                    selected_index: 0,
                });
            }
        } else {
            app.open_context_menu();
        }
        return false;
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
            if let Some(pty) = app.pty.as_mut() {
                pty.write_key(key);
            }
        }
    }
    false
}

fn handle_context_menu_key(app: &mut App, key: KeyEvent, terminal_rect: Rect) -> bool {
    let Some(menu) = app.context_menu.as_mut() else {
        return false;
    };
    match key.code {
        KeyCode::Esc => app.context_menu = None,
        KeyCode::Up => {
            if !menu.actions.is_empty() {
                menu.selected_index =
                    wrap_index(menu.selected_index as isize - 1, menu.actions.len());
            }
        }
        KeyCode::Down => {
            if !menu.actions.is_empty() {
                menu.selected_index =
                    wrap_index(menu.selected_index as isize + 1, menu.actions.len());
            }
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let action = menu.actions.get(menu.selected_index).cloned();
            app.context_menu = None;
            if let Some(action) = action {
                execute_context_action(app, action, terminal_rect);
            }
        }
        _ => {}
    }
    false
}

fn handle_input_prompt_key(app: &mut App, key: KeyEvent) -> bool {
    let Some(prompt) = app.input_prompt.as_mut() else {
        return false;
    };
    match key.code {
        KeyCode::Esc => app.input_prompt = None,
        KeyCode::Enter => {
            let prompt = app.input_prompt.take();
            if let Some(prompt) = prompt {
                execute_input_prompt(app, prompt);
            }
        }
        KeyCode::Backspace => {
            prompt.value.pop();
        }
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            prompt.value.push(ch);
        }
        _ => {}
    }
    false
}

fn handle_mouse(app: &mut App, mouse: MouseEvent, full: Rect, terminal_rect: Rect) -> bool {
    if app.show_hotkeys {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            app.show_hotkeys = false;
        }
        return false;
    }
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
            if rect_contains(terminal_rect, mouse.column, mouse.row) {
                if let Some(pty) = app.pty.as_mut() {
                    pty.handle_mouse(mouse, terminal_rect);
                }
            }
        }
        Mode::Switcher => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if rect_contains(
                    hotkeys_button_rect(header_area(full)),
                    mouse.column,
                    mouse.row,
                ) =>
            {
                app.show_hotkeys = true;
            }
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
                if !rect_contains(terminal_rect, mouse.column, mouse.row) {
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
        }) => {
            match create_terminal(project_id.as_deref(), group_id.as_deref()) {
                Ok(created) => {
                    /*
                    CDXC:GhostexTui 2026-05-25-18:05:
                    Selecting "Create new terminal" should immediately attach to
                    the created session. Refresh the macOS sidebar inventory first
                    so attachment still uses the same sidebar-ordered session model
                    and `zmx attach` path as selecting an existing session.
                    */
                    app.status.clear();
                    app.refresh_sessions(false);
                    if let Some(session_id) = created
                        .session
                        .and_then(|session| session.ghostex_id.or(session.session_id))
                    {
                        if let Some(row_index) = app.row_index_for_session_id(&session_id) {
                            app.selected_row_index = row_index;
                            app.sync_selected_session_index_from_row();
                            if let Some(session) = app.session_by_id(&session_id).cloned() {
                                app.attach(session, terminal_rect);
                            }
                        } else {
                            app.status = "Created terminal, but it was not found in the refreshed session list.".to_string();
                        }
                    }
                }
                Err(err) => {
                    app.status = format!("Could not create terminal: {err}");
                }
            }
        }
        None => {}
    }
}

fn session_context_actions(session: &SessionItem) -> Vec<ContextAction> {
    let favorite = !session.is_favorite.unwrap_or(false);
    let sleeping = session.status != "sleep";
    let mut actions = vec![
        ContextAction {
            danger: false,
            label: "Attach".to_string(),
            kind: ContextActionKind::SessionAttach(session.clone()),
        },
        ContextAction {
            danger: false,
            label: "Rename".to_string(),
            kind: ContextActionKind::RenameSession(session.clone()),
        },
        ContextAction {
            danger: false,
            label: if favorite { "Favorite" } else { "Unfavorite" }.to_string(),
            kind: ContextActionKind::SessionFavorite {
                session: session.clone(),
                favorite,
            },
        },
        ContextAction {
            danger: false,
            label: if sleeping { "Sleep" } else { "Wake" }.to_string(),
            kind: ContextActionKind::SessionSleep {
                session: session.clone(),
                sleeping,
            },
        },
    ];
    if session
        .resume_command
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || session
            .resume_fallback_command
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        actions.push(ContextAction {
            danger: false,
            label: "Copy resume".to_string(),
            kind: ContextActionKind::CopyResumeCommand(session.clone()),
        });
    }
    if session
        .attach_command
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        actions.push(ContextAction {
            danger: false,
            label: "Copy attach command".to_string(),
            kind: ContextActionKind::CopyAttachCommand(session.clone()),
        });
    }
    actions.extend([
        ContextAction {
            danger: false,
            label: "Fork".to_string(),
            kind: ContextActionKind::SessionFork(session.clone()),
        },
        ContextAction {
            danger: false,
            label: "Full reload".to_string(),
            kind: ContextActionKind::SessionFullReload(session.clone()),
        },
        ContextAction {
            danger: true,
            label: "Close".to_string(),
            kind: ContextActionKind::SessionClose(session.clone()),
        },
    ]);
    actions
}

fn project_context_actions(
    header: &ProjectHeader,
    sessions: Vec<SessionItem>,
) -> Vec<ContextAction> {
    let all_sleeping =
        !sessions.is_empty() && sessions.iter().all(|session| session.status == "sleep");
    let mut actions = vec![
        ContextAction {
            danger: false,
            label: "Create new terminal".to_string(),
            kind: ContextActionKind::CreateTerminal {
                project_id: header.project_id.clone(),
                group_id: header.group_id.clone(),
            },
        },
        ContextAction {
            danger: false,
            label: "Focus".to_string(),
            kind: ContextActionKind::FocusGroup(header.clone()),
        },
    ];
    if header
        .path
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        actions.extend([
            ContextAction {
                danger: false,
                label: "Copy Path".to_string(),
                kind: ContextActionKind::CopyProjectPath(header.clone()),
            },
            ContextAction {
                danger: false,
                label: "Open in Finder".to_string(),
                kind: ContextActionKind::OpenProjectInFinder(header.clone()),
            },
        ]);
    }
    actions.extend([
        ContextAction {
            danger: false,
            label: if all_sleeping { "Wake" } else { "Sleep" }.to_string(),
            kind: ContextActionKind::ProjectSleep {
                header: header.clone(),
                sleeping: !all_sleeping,
            },
        },
        ContextAction {
            danger: false,
            label: "Full reload".to_string(),
            kind: ContextActionKind::ProjectFullReload(header.clone()),
        },
    ]);
    if header.project_id.is_some() {
        actions.extend([
            ContextAction {
                danger: false,
                label: "Move Project Up".to_string(),
                kind: ContextActionKind::ProjectMove {
                    project_id: header.project_id.clone(),
                    direction: "up",
                },
            },
            ContextAction {
                danger: false,
                label: "Move Project Down".to_string(),
                kind: ContextActionKind::ProjectMove {
                    project_id: header.project_id.clone(),
                    direction: "down",
                },
            },
        ]);
    }
    actions.push(ContextAction {
        danger: true,
        label: "Close Sessions".to_string(),
        kind: ContextActionKind::ProjectCloseSessions(header.clone()),
    });
    actions
}

fn execute_context_action(app: &mut App, action: ContextAction, terminal_rect: Rect) {
    let result = match action.kind {
        ContextActionKind::SessionAttach(session) => {
            app.attach(session, terminal_rect);
            Ok(())
        }
        ContextActionKind::RenameSession(session) => {
            app.input_prompt = Some(InputPrompt {
                title: "Rename session".to_string(),
                value: session.title.clone(),
                action: InputPromptAction::RenameSession(session),
            });
            Ok(())
        }
        ContextActionKind::SessionFavorite { session, favorite } => run_ghostex_cli(&[
            "favorite-session".to_string(),
            "--session-id".to_string(),
            session.session_id,
            favorite.to_string(),
        ])
        .map(|_| ()),
        ContextActionKind::SessionSleep { session, sleeping } => run_ghostex_cli(&[
            "sleep-session".to_string(),
            "--session-id".to_string(),
            session.session_id,
            sleeping.to_string(),
        ])
        .map(|_| ()),
        ContextActionKind::CopyResumeCommand(session) => copy_text(
            session
                .resume_command
                .or(session.resume_fallback_command)
                .unwrap_or_default()
                .as_str(),
        ),
        ContextActionKind::CopyAttachCommand(session) => {
            copy_text(session.attach_command.unwrap_or_default().as_str())
        }
        ContextActionKind::SessionFork(session) => run_session_command("fork-session", &session),
        ContextActionKind::SessionFullReload(session) => {
            run_session_command("reload-session", &session)
        }
        ContextActionKind::SessionClose(session) => run_session_command("close-session", &session),
        ContextActionKind::CreateTerminal {
            project_id,
            group_id,
        } => create_terminal(project_id.as_deref(), group_id.as_deref()).map(|_| ()),
        ContextActionKind::FocusGroup(header) => {
            if let Some(group_id) = header.group_id {
                run_ghostex_cli(&["focus-group".to_string(), group_id]).map(|_| ())
            } else if let Some(project_id) = header.project_id {
                run_ghostex_cli(&[
                    "switch-project".to_string(),
                    "--project-id".to_string(),
                    project_id,
                ])
                .map(|_| ())
            } else {
                Ok(())
            }
        }
        ContextActionKind::CopyProjectPath(header) => {
            copy_text(header.path.unwrap_or_default().as_str())
        }
        ContextActionKind::OpenProjectInFinder(header) => {
            open_in_finder(header.path.unwrap_or_default().as_str())
        }
        ContextActionKind::ProjectSleep { header, sleeping } => {
            let sessions = app.project_sessions(&header);
            run_project_session_command(&sessions, "sleep-session", Some(sleeping))
        }
        ContextActionKind::ProjectFullReload(header) => {
            let sessions = app.project_sessions(&header);
            run_project_session_command(&sessions, "reload-session", None)
        }
        ContextActionKind::ProjectCloseSessions(header) => {
            let sessions = app.project_sessions(&header);
            run_project_session_command(&sessions, "close-session", None)
        }
        ContextActionKind::ProjectMove {
            project_id,
            direction,
        } => {
            if let Some(project_id) = project_id {
                run_ghostex_cli(&[
                    "move-project".to_string(),
                    "--project-id".to_string(),
                    project_id,
                    "--direction".to_string(),
                    direction.to_string(),
                ])
                .map(|_| ())
            } else {
                Ok(())
            }
        }
    };
    match result {
        Ok(()) => {
            app.status.clear();
            app.refresh_sessions(false);
        }
        Err(err) => app.status = format!("Action failed: {err}"),
    }
}

fn execute_input_prompt(app: &mut App, prompt: InputPrompt) {
    let result = match prompt.action {
        InputPromptAction::RenameSession(session) => run_ghostex_cli(&[
            "rename-session".to_string(),
            "--session-id".to_string(),
            session.session_id,
            "--title".to_string(),
            prompt.value,
        ])
        .map(|_| ()),
    };
    match result {
        Ok(()) => {
            app.status.clear();
            app.refresh_sessions(false);
        }
        Err(err) => app.status = format!("Action failed: {err}"),
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
    let mut args = vec!["create-session".to_string()];
    if let Some(project_id) = project_id.filter(|value| !value.trim().is_empty()) {
        args.extend(["--project-id".to_string(), project_id.to_string()]);
    }
    if let Some(group_id) = group_id.filter(|value| !value.trim().is_empty()) {
        args.extend(["--group-id".to_string(), group_id.to_string()]);
    }
    let output = run_ghostex_cli(&args)?;
    serde_json::from_slice(&output).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn run_session_command(command: &str, session: &SessionItem) -> io::Result<()> {
    run_ghostex_cli(&[
        command.to_string(),
        "--session-id".to_string(),
        session.session_id.clone(),
    ])
    .map(|_| ())
}

fn acknowledge_session_attention(session: &SessionItem) -> io::Result<()> {
    run_ghostex_cli(&[
        "acknowledge-session-attention".to_string(),
        "--session-id".to_string(),
        session.session_id.clone(),
    ])
    .map(|_| ())
}

fn run_project_session_command(
    sessions: &[SessionItem],
    command: &str,
    boolean: Option<bool>,
) -> io::Result<()> {
    for session in sessions {
        let mut args = vec![
            command.to_string(),
            "--session-id".to_string(),
            session.session_id.clone(),
        ];
        if let Some(value) = boolean {
            args.push(value.to_string());
        }
        run_ghostex_cli(&args)?;
    }
    Ok(())
}

fn copy_text(text: &str) -> io::Result<()> {
    let mut child = Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, "pbcopy failed"))
    }
}

fn open_in_finder(path: &str) -> io::Result<()> {
    let status = Command::new("open").arg(path).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, "open failed"))
    }
}

fn run_ghostex_cli(args: &[String]) -> io::Result<Vec<u8>> {
    let mut command = ghostex_cli_command();
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    let output = Command::new("/bin/zsh").arg("-lc").arg(command).output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(output.stdout)
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
                path: session.project_path.clone(),
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
        rows.push(SwitchRow::Project(ProjectHeader {
            project_id: group.project_id.clone(),
            group_id: group.group_id.clone(),
            name: group.name.clone(),
            path: group.path.clone(),
        }));
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
    let height = full.height.min(3);
    Rect::new(
        full.x,
        full.y + full.height.saturating_sub(height),
        full.width,
        height,
    )
}

fn terminal_area(full: Rect) -> Rect {
    let header = header_area(full);
    Rect::new(
        full.x,
        full.y,
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

fn hotkeys_button_rect(header: Rect) -> Rect {
    let switch_width = 12u16.min(header.width);
    let available_width = header.width.saturating_sub(switch_width);
    let width = 14u16.min(available_width);
    Rect::new(
        header.x,
        header.y + 1,
        width,
        header.height.saturating_sub(1).min(2),
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

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session(project_id: &str, title: &str) -> SessionItem {
        SessionItem {
            activity: None,
            agent: Some("codex".to_string()),
            project_id: Some(project_id.to_string()),
            group_id: Some(format!("{project_id}-group")),
            attach_command: Some(format!("zmx attach {project_id}-{title}")),
            is_favorite: Some(false),
            project_name: Some(project_id.to_string()),
            project_path: Some(format!("/{project_id}")),
            resume_command: None,
            resume_fallback_command: None,
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
            context_menu: None,
            input_prompt: None,
            show_hotkeys: false,
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
                path: Some("/alpha".to_string()),
                sessions: vec![test_session("alpha", "one"), test_session("alpha", "two")],
            },
            ProjectGroup {
                project_id: Some("beta".to_string()),
                group_id: Some("beta-group".to_string()),
                name: "beta".to_string(),
                path: Some("/beta".to_string()),
                sessions: vec![test_session("beta", "one"), test_session("beta", "two")],
            },
            ProjectGroup {
                project_id: Some("gamma".to_string()),
                group_id: Some("gamma-group".to_string()),
                name: "gamma".to_string(),
                path: Some("/gamma".to_string()),
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
            path: Some("/alpha".to_string()),
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
