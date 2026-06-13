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
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use herdr::{config, events, input, layout, pane, selection, terminal, terminal_theme};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde::Deserialize;
use tokio::sync::{mpsc as tokio_mpsc, Notify};
use unicode_width::UnicodeWidthChar;

const POLL_INTERVAL: Duration = Duration::from_millis(16);
const SESSION_LIST_REFRESH: Duration = Duration::from_secs(5);
const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(30);
/*
CDXC:GhostexTui 2026-06-13-23:12:
The session switcher must keep held Up/Down navigation at a normal list-repeat cadence instead of applying every terminal repeat event, while first presses remain immediate and attached terminal input remains unthrottled.
*/
const SWITCHER_VERTICAL_NAV_REPEAT_INTERVAL: Duration = Duration::from_millis(90);
const TERMINAL_SCROLLBACK_BYTES: usize = config::DEFAULT_SCROLLBACK_LIMIT_BYTES;
const MOUSE_SCROLL_LINES: usize = 3;
const GHOSTEX_TUI_TERM: &str = "xterm-256color";
const GHOSTEX_TUI_COLORTERM: &str = "truecolor";
const HEADER_CONTROL_WIDTH: u16 = 9;
const HEADER_PROJECT_LABEL_WIDTH: usize = 7;
/*
CDXC:GhostexTuiColors 2026-06-09-09:59:
Ghostex TUI backgrounds should read as neutral gray/cool gray rather than purple, with blue reserved for active accents, borders, and badges.
Keep the wrapper palette local and semantic so switcher, header, menu, and overlay surfaces stay visually consistent.
*/
const TUI_BG: Color = Color::Rgb(22, 24, 28);
const TUI_SURFACE: Color = Color::Rgb(34, 39, 46);
const TUI_SELECTED_BG: Color = Color::Rgb(45, 54, 64);
const TUI_RULE: Color = Color::Rgb(70, 82, 96);
const TUI_ACCENT_BLUE: Color = Color::Rgb(88, 166, 255);
const TUI_TEXT: Color = Color::Rgb(230, 237, 243);
const TUI_SUBTLE_TEXT: Color = Color::Rgb(139, 148, 158);
const TUI_DANGER: Color = Color::Rgb(255, 123, 114);
const TUI_AGENT_CLAUDE_ORANGE: Color = Color::Rgb(255, 136, 76);
const TUI_AGENT_PERIWINKLE: Color = Color::Rgb(139, 154, 255);
const TUI_AGENT_OPENCODE_BLUE: Color = Color::Rgb(126, 166, 203);
const TUI_AGENT_FACTORY_ORANGE: Color = Color::Rgb(255, 136, 36);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachedSelectionAutoscrollDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitcherVerticalNavKey {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy)]
struct SwitcherVerticalNavRepeat {
    key: SwitcherVerticalNavKey,
    last_accepted: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachedSelectionAutoscroll {
    direction: AttachedSelectionAutoscrollDirection,
    last_mouse_screen_col: u16,
    last_mouse_screen_row: u16,
    terminal_rect: Rect,
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
        let shell_command = attach_shell_command(session);
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
        shell that launched `gx`. Force a real terminal identity so Codex CLI,
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

    fn forward_mouse_button(&mut self, mouse: MouseEvent, terminal_rect: Rect) -> bool {
        let (column, row) = attached_terminal_mouse_cell(mouse, terminal_rect);
        let Some(bytes) =
            self.runtime
                .encode_mouse_button(mouse.kind, column, row, mouse.modifiers)
        else {
            return false;
        };
        self.runtime.scroll_reset();
        self.write_input(bytes);
        true
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
                let (column, row) = attached_terminal_mouse_cell(mouse, terminal_rect);
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
        let bytes = attached_terminal_key_override_bytes(key)
            .unwrap_or_else(|| self.runtime.encode_terminal_key(key.into()));
        if !bytes.is_empty() {
            self.runtime.scroll_reset();
            self.write_input(bytes);
        }
    }

    async fn send_paste(&mut self, text: String) {
        self.runtime.scroll_reset();
        let _ = self.runtime.send_paste(text).await;
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
    selection: Option<selection::Selection>,
    selection_autoscroll: Option<AttachedSelectionAutoscroll>,
    selection_autoscroll_deadline: Option<Instant>,
    switcher_vertical_nav_repeat: Option<SwitcherVerticalNavRepeat>,
    mode: Mode,
    switch_scroll: usize,
    last_refresh: Instant,
    known_attention_session_keys: HashSet<String>,
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
            selection: None,
            selection_autoscroll: None,
            selection_autoscroll_deadline: None,
            switcher_vertical_nav_repeat: None,
            mode: Mode::Switcher,
            switch_scroll: 0,
            last_refresh: Instant::now() - SESSION_LIST_REFRESH,
            known_attention_session_keys: HashSet::new(),
            has_loaded_session_statuses: false,
            status: String::new(),
        };
        app.refresh_sessions(false);
        /*
        CDXC:GhostexTui 2026-05-25-15:11:
        Bare `gx` should open on the project/session switcher, not auto-attach
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
                let next_attention_session_keys = attention_session_keys(&sessions);
                if bell_on_new_attention
                    && self.has_loaded_session_statuses
                    && next_attention_session_keys
                        .difference(&self.known_attention_session_keys)
                        .next()
                        .is_some()
                {
                    emit_terminal_bell();
                }
                self.known_attention_session_keys = next_attention_session_keys;
                self.has_loaded_session_statuses = true;
                let selected_session_key = self.selected_session_at_row().map(session_identity_key);
                let active_session_key = self.active_session.as_ref().map(session_identity_key);
                self.groups = group_sessions(sessions);
                self.rows = switch_rows(&self.groups);
                if let Some(selected_session_key) = selected_session_key {
                    if let Some(row_index) = self.row_index_for_session_key(&selected_session_key) {
                        self.selected_row_index = row_index;
                    } else {
                        self.clamp_selected_row_to_selectable();
                    }
                } else {
                    self.clamp_selected_row_to_selectable();
                }
                self.sync_selected_session_index_from_row();
                if let Some(active_session_key) = active_session_key {
                    if let Some(session) = self.session_by_key(&active_session_key).cloned() {
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

    fn clear_selection(&mut self) {
        self.selection = None;
        self.stop_selection_autoscroll();
    }

    fn stop_selection_autoscroll(&mut self) {
        self.selection_autoscroll = None;
        self.selection_autoscroll_deadline = None;
    }

    fn tick_selection_autoscroll(&mut self, now: Instant, terminal_rect: Rect) {
        let Some(deadline) = self.selection_autoscroll_deadline else {
            return;
        };
        if now < deadline {
            return;
        }
        let Some(autoscroll) = self.selection_autoscroll else {
            self.selection_autoscroll_deadline = None;
            return;
        };
        if autoscroll.terminal_rect != terminal_rect {
            self.stop_selection_autoscroll();
            return;
        }
        if !self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.is_dragging())
        {
            self.stop_selection_autoscroll();
            return;
        }
        let Some(metrics) = self
            .pty
            .as_ref()
            .and_then(|pty| pty.runtime.scroll_metrics())
        else {
            self.stop_selection_autoscroll();
            return;
        };
        match autoscroll.direction {
            AttachedSelectionAutoscrollDirection::Up => {
                if metrics.offset_from_bottom >= metrics.max_offset_from_bottom {
                    self.stop_selection_autoscroll();
                    return;
                }
                if let Some(pty) = self.pty.as_ref() {
                    pty.runtime.scroll_up(1);
                }
            }
            AttachedSelectionAutoscrollDirection::Down => {
                if metrics.offset_from_bottom == 0 {
                    self.stop_selection_autoscroll();
                    return;
                }
                if let Some(pty) = self.pty.as_ref() {
                    pty.runtime.scroll_down(1);
                }
            }
        }
        let metrics = self
            .pty
            .as_ref()
            .and_then(|pty| pty.runtime.scroll_metrics());
        if let Some(selection) = self.selection.as_mut() {
            selection.drag(
                autoscroll.last_mouse_screen_col,
                autoscroll.last_mouse_screen_row,
                terminal_rect,
                metrics,
            );
        }
        self.selection_autoscroll_deadline = Some(now + SELECTION_AUTOSCROLL_INTERVAL);
    }

    fn attach(&mut self, session: SessionItem, area: Rect) {
        /*
        CDXC:GhostexTui 2026-05-26-13:03:
        Attaching to an attention session from Ghostex TUI means the user has seen
        that shared attention event. Acknowledge through the Ghostex CLI bridge
        so the desktop sidebar and any other TUI clients clear the same event.
        */
        if session_activity(&session) == Some(SessionActivity::Attention) {
            let _ = acknowledge_session_attention(&session);
            self.known_attention_session_keys
                .remove(&session_identity_key(&session));
        }
        match PtySession::spawn(&session, area) {
            Ok(pty) => {
                self.pty = Some(pty);
                self.clear_selection();
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

    fn session_by_key(&self, key: &str) -> Option<&SessionItem> {
        self.rows.iter().find_map(|row| match row {
            SwitchRow::Session(session) if session_identity_key(session) == key => Some(session),
            _ => None,
        })
    }

    fn row_index_for_session_key(&self, key: &str) -> Option<usize> {
        self.rows.iter().position(
            |row| matches!(row, SwitchRow::Session(session) if session_identity_key(session) == key),
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

    fn should_handle_switcher_vertical_nav(
        &mut self,
        key: SwitcherVerticalNavKey,
        kind: KeyEventKind,
        now: Instant,
    ) -> bool {
        if kind == KeyEventKind::Release {
            if self
                .switcher_vertical_nav_repeat
                .is_some_and(|repeat| repeat.key == key)
            {
                self.switcher_vertical_nav_repeat = None;
            }
            return false;
        }

        if let Some(repeat) = self.switcher_vertical_nav_repeat {
            if repeat.key == key
                && now.saturating_duration_since(repeat.last_accepted)
                    < SWITCHER_VERTICAL_NAV_REPEAT_INTERVAL
            {
                return false;
            }
        }

        self.switcher_vertical_nav_repeat = Some(SwitcherVerticalNavRepeat {
            key,
            last_accepted: now,
        });
        true
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

struct TerminalGuard {
    reset_modify_other_keys: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange,
            PushKeyboardEnhancementFlags(input::ime_compatible_keyboard_enhancement_flags())
        )?;
        /*
        CDXC:GhostexTui 2026-06-07-16:34:
        Shift+Enter in attached Ghostex TUI sessions must reach agent CLIs as
        LF/Ctrl+J. Match Herdr's host keyboard negotiation so terminals that do
        not report modified Enter through Kitty flags still expose it through
        xterm modifyOtherKeys where Herdr already knows the host is parseable.
        */
        let modify_other_keys_mode = input::host_modify_other_keys_mode(
            std::env::var("TMUX").is_ok(),
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            std::env::var_os("WEZTERM_PANE").is_some(),
        );
        if let Some(mode) = modify_other_keys_mode {
            io::stdout().write_all(mode.set_sequence())?;
            io::stdout().flush()?;
        }
        Ok(Self {
            reset_modify_other_keys: modify_other_keys_mode.is_some(),
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.reset_modify_other_keys {
            let _ = io::stdout().write_all(b"\x1b[>4;0m");
            let _ = io::stdout().flush();
        }
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
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
        app.tick_selection_autoscroll(Instant::now(), terminal_rect);
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
                Event::Paste(text) => handle_paste(&mut app, text).await,
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
    The header title may wrap onto the second content row. Keep the switch
    affordance two rows tall and label it as "switch session" with one word per
    row.

    CDXC:GhostexTui 2026-06-09-09:22:
    When the user is already on the switcher, the top-right control should
    become the exit affordance and read "Quit" over "Ghostex" instead of
    offering to switch sessions again.

    CDXC:GhostexTui 2026-06-09-09:22:
    The switcher quit label should keep "Quit" on the first row and "Ghostex"
    on the second row.

    CDXC:GhostexTui 2026-05-25-18:37:
    The TUI title bar sits at the bottom of the screen, so its separator must
    render on the top edge instead of below the bar to divide session output
    from Ghostex controls.

    CDXC:GhostexTui 2026-06-09-09:22:
    In switcher mode, replace the left title area with a "Help &" over
    "Hotkeys" button that opens the shortcuts overlay. The switcher already
    shows session context, so repeating the Ghostex/title label is less useful
    than discoverable TUI controls.

    CDXC:GhostexTui 2026-05-26-03:39:
    Attached-view working/attention totals should remain compact dot counters;
    the 2026-06-07 badge requirement owns their left-side placement.

    CDXC:GhostexTui 2026-06-07-15:47:
    Attached terminal chrome needs a fixed 9-column, two-row project badge on
    the left, mirroring the switch control on the right. Replace the Ghostex
    brand with the first seven display columns of the active project name and
    put the activity totals directly below it so the center title remains only
    session context.
    */
    let header_style = Style::default().bg(TUI_BG);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new("").style(header_style), area);
    let switch = switch_button_rect(area);
    if app.mode == Mode::Switcher {
        frame.render_widget(
            Paragraph::new("Help &\nHotkeys")
                .style(
                    Style::default()
                        .fg(Color::White)
                        .bg(TUI_SELECTED_BG)
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
    if app.mode == Mode::Attached {
        /*
        CDXC:GhostexTui 2026-05-25-17:23:
        Attached-view activity totals should be compact dot counters without
        the literal words "working" or "attention"; the dot colors carry the
        same meaning as the macOS sidebar indicators.
        */
        let project_badge = project_badge_rect(area);
        if project_badge.height > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    project_badge_label(app.active_session.as_ref()),
                    Style::default()
                        .fg(Color::Black)
                        .bg(TUI_ACCENT_BLUE)
                        .add_modifier(Modifier::BOLD),
                ))),
                Rect::new(project_badge.x, project_badge.y, project_badge.width, 1),
            );
        }
        if project_badge.height > 1 {
            frame.render_widget(
                Paragraph::new(Line::from(activity_count_badge_spans(
                    app.activity_count(SessionActivity::Working),
                    app.activity_count(SessionActivity::Attention),
                    TUI_BG,
                )))
                .style(header_style)
                .alignment(Alignment::Center),
                Rect::new(project_badge.x, project_badge.y + 1, project_badge.width, 1),
            );
        }
        let title_gap = u16::from(project_badge.width > 0);
        let title_x = area
            .x
            .saturating_add(project_badge.width)
            .saturating_add(title_gap);
        let title_width = switch.x.saturating_sub(title_x);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                title.to_string(),
                header_style.fg(Color::White).add_modifier(Modifier::BOLD),
            )))
            .style(header_style)
            .wrap(Wrap { trim: false }),
            Rect::new(
                title_x,
                area.y + 1,
                title_width,
                area.height.saturating_sub(1),
            ),
        );
    }
    frame.render_widget(
        Paragraph::new(match app.mode {
            Mode::Attached => "switch\nsession",
            Mode::Switcher => "Quit\nGhostex",
        })
        .style(
            Style::default()
                .fg(Color::White)
                .bg(TUI_SELECTED_BG)
                .add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::LEFT)),
        switch,
    );
    if area.height > 0 {
        frame.render_widget(
            Paragraph::new("─".repeat(area.width as usize))
                .style(Style::default().fg(TUI_RULE).bg(TUI_BG)),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
}

fn render_terminal(frame: &mut Frame, app: &mut App, area: Rect) {
    if let Some(pty) = app.pty.as_ref() {
        pty.runtime.render(frame, area, true);
        render_terminal_selection(frame, app, pty, area);
    } else {
        frame.render_widget(
            Paragraph::new(app.status.as_str()).style(Style::default().fg(TUI_DANGER).bg(TUI_BG)),
            area,
        );
    }
}

fn render_terminal_selection(frame: &mut Frame, app: &App, pty: &PtySession, area: Rect) {
    /*
     * CDXC:GhostexTui 2026-06-07-16:19:
     * Attached Ghostex TUI sessions need host-side text selection because the
     * wrapper owns mouse capture while rendering a single Ghostty-backed PTY.
     * Highlight selected viewport cells after terminal rendering so selection
     * is visible without changing the child process output.
     */
    let Some(selection) = app.selection.as_ref() else {
        return;
    };
    if !selection.is_visible() || selection.pane_id != pty.pane_id {
        return;
    }
    let metrics = pty.runtime.scroll_metrics();
    let buf = frame.buffer_mut();
    for y in 0..area.height {
        for x in 0..area.width {
            if selection.contains(y, x, metrics) {
                let cell = &mut buf[(area.x + x, area.y + y)];
                cell.set_style(Style::default().fg(Color::Black).bg(TUI_ACCENT_BLUE));
            }
        }
    }
}

fn activity_dot_span(session: &SessionItem, bg: Color) -> Span<'static> {
    match session_activity(session) {
        Some(activity) => Span::styled(" ●", Style::default().fg(activity_color(activity)).bg(bg)),
        None => Span::styled("  ", Style::default().bg(bg)),
    }
}

fn activity_count_badge_spans(
    working_count: usize,
    attention_count: usize,
    bg: Color,
) -> Vec<Span<'static>> {
    let working_count = compact_activity_count(working_count);
    let attention_count = compact_activity_count(attention_count);
    let spaced = format!("● {working_count} ● {attention_count}");
    let count_gap = if spaced.chars().count() <= HEADER_CONTROL_WIDTH as usize {
        " "
    } else {
        ""
    };
    vec![
        Span::styled("●", Style::default().fg(WORKING_COLOR).bg(bg)),
        Span::raw(count_gap),
        Span::styled(working_count, Style::default().fg(Color::White).bg(bg)),
        Span::raw(" "),
        Span::styled("●", Style::default().fg(ATTENTION_COLOR).bg(bg)),
        Span::raw(count_gap),
        Span::styled(attention_count, Style::default().fg(Color::White).bg(bg)),
    ]
}

fn compact_activity_count(count: usize) -> String {
    count.min(999).to_string()
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

fn attention_session_keys(sessions: &[SessionItem]) -> HashSet<String> {
    sessions
        .iter()
        .filter(|session| session_activity(session) == Some(SessionActivity::Attention))
        .map(session_identity_key)
        .collect()
}

fn attach_shell_command(session: &SessionItem) -> String {
    attach_shell_command_with_cli(&ghostex_cli_command(), session)
}

fn attach_shell_command_with_cli(cli_command: &str, session: &SessionItem) -> String {
    /*
     * CDXC:GhostexTui 2026-06-04-03:27:
     * Ghostex TUI attaches by launching the shared CLI inside Herdr's PTY.
     * Include projectId whenever the sidebar inventory provides it so attach
     * resolves the same full S/P/G zmx session as macOS, Electron, mobile, and
     * gxserver lifecycle actions instead of routing by a bare G id.
     */
    let mut command = format!(
        "{cli_command} attach --session-id {}",
        shell_quote(&session.session_id)
    );
    if let Some(project_id) = session
        .project_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        command.push_str(" --project-id ");
        command.push_str(&shell_quote(project_id));
    }
    command
}

fn session_identity_key(session: &SessionItem) -> String {
    session_identity_key_parts(session.project_id.as_deref(), &session.session_id)
}

fn session_identity_key_parts(project_id: Option<&str>, session_id: &str) -> String {
    /*
     * CDXC:GhostexTui 2026-06-04-03:27:
     * TUI switcher bookkeeping must preserve the project/session pair because
     * gxserver G ids are project-scoped. Keep duplicate bare session ids
     * distinct across refresh, active-session sync, attention acknowledgement,
     * and create-then-attach flows.
     */
    let project_id = project_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    if project_id.is_empty() {
        session_id.to_string()
    } else {
        format!("{project_id}/{session_id}")
    }
}

fn emit_terminal_bell() {
    let _ = io::stdout().write_all(b"\x07");
    let _ = io::stdout().flush();
}

fn render_switcher(frame: &mut Frame, app: &mut App, area: Rect) {
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new("").style(Style::default().bg(TUI_BG)), area);
    let content = switcher_content_rect(area);
    app.keep_selected_visible(content);
    let visible_rows = app
        .rows
        .iter()
        .enumerate()
        .skip(app.switch_scroll)
        .take(content.height as usize)
        .map(|(idx, row)| match row {
            SwitchRow::Project(project) => {
                let selected = idx == app.selected_row_index;
                let bg = if selected { TUI_SELECTED_BG } else { TUI_BG };
                ListItem::new(Line::from(Span::styled(
                    project.name.clone(),
                    Style::default()
                        .fg(TUI_ACCENT_BLUE)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                )))
                .style(Style::default().bg(bg))
            }
            SwitchRow::NewTerminal { .. } => {
                let selected = idx == app.selected_row_index;
                let bg = if selected { TUI_SELECTED_BG } else { TUI_BG };
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
                        .fg(TUI_TEXT)
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
                let bg = if selected { TUI_SELECTED_BG } else { TUI_BG };
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
                .border_style(Style::default().fg(TUI_ACCENT_BLUE))
                .style(Style::default().bg(TUI_BG)),
        )
        .style(Style::default().bg(TUI_BG))
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
                TUI_SELECTED_BG
            } else {
                TUI_SURFACE
            };
            let fg = if action.danger {
                TUI_DANGER
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
                    .border_style(Style::default().fg(TUI_ACCENT_BLUE))
                    .style(Style::default().bg(TUI_SURFACE)),
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
            .style(Style::default().fg(Color::White).bg(TUI_SURFACE))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(TUI_ACCENT_BLUE)),
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
            Span::styled("Ctrl+Q", Style::default().fg(TUI_ACCENT_BLUE)),
            Span::raw("  Quit Ghostex"),
        ]),
        Line::from(vec![
            Span::styled("Ctrl+S", Style::default().fg(TUI_ACCENT_BLUE)),
            Span::raw("  Open switcher from attached session"),
        ]),
        Line::from(vec![
            Span::styled("Ctrl+K", Style::default().fg(TUI_ACCENT_BLUE)),
            Span::raw("  Open context menu"),
        ]),
        Line::from(vec![
            Span::styled("Esc", Style::default().fg(TUI_ACCENT_BLUE)),
            Span::raw("     Close overlay/menu, or return to session"),
        ]),
        Line::from(vec![
            Span::styled("Up/Down", Style::default().fg(TUI_ACCENT_BLUE)),
            Span::raw(" Move selection"),
        ]),
        Line::from(vec![
            Span::styled("Left/Right", Style::default().fg(TUI_ACCENT_BLUE)),
            Span::raw(" Jump projects"),
        ]),
        Line::from(vec![
            Span::styled("PgUp/PgDn", Style::default().fg(TUI_ACCENT_BLUE)),
            Span::raw(" Jump 5 rows"),
        ]),
        Line::from(vec![
            Span::styled("Enter/Space", Style::default().fg(TUI_ACCENT_BLUE)),
            Span::raw(" Attach, create, or confirm"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Click anywhere or press Esc to close",
            Style::default().fg(TUI_SUBTLE_TEXT),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(Color::White).bg(TUI_SURFACE))
            .block(
                Block::default()
                    .title(" Hotkeys ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(TUI_ACCENT_BLUE)),
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
            app.clear_selection();
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
            KeyCode::Up => {
                if app.should_handle_switcher_vertical_nav(
                    SwitcherVerticalNavKey::Up,
                    key.kind,
                    Instant::now(),
                ) {
                    app.select_delta(-1);
                }
            }
            KeyCode::Down => {
                if app.should_handle_switcher_vertical_nav(
                    SwitcherVerticalNavKey::Down,
                    key.kind,
                    Instant::now(),
                ) {
                    app.select_delta(1);
                }
            }
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
                app.clear_selection();
                app.mode = Mode::Switcher;
                app.refresh_sessions(false);
                return false;
            }
            app.clear_selection();
            if let Some(pty) = app.pty.as_mut() {
                pty.write_key(key);
            }
        }
    }
    false
}

async fn handle_paste(app: &mut App, text: String) {
    /*
     * CDXC:GhostexTui 2026-06-07-16:47:
     * Enabling Herdr's keyboard protocol setup also enables bracketed paste on
     * the host terminal. Forward paste events through the attached runtime so
     * child shells and agent CLIs keep receiving normal or bracketed paste
     * payloads according to their own terminal mode.
     */
    if let Some(prompt) = app.input_prompt.as_mut() {
        prompt.value.push_str(&text);
        return;
    }
    if app.show_hotkeys || app.context_menu.is_some() || app.mode != Mode::Attached {
        return;
    }
    app.clear_selection();
    if let Some(pty) = app.pty.as_mut() {
        pty.send_paste(text).await;
    }
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
                app.clear_selection();
                app.mode = Mode::Switcher;
                app.refresh_sessions(false);
            }
            let terminal_mouse_event = rect_contains(terminal_rect, mouse.column, mouse.row);
            let selection_drag_event = app.selection.is_some()
                && matches!(
                    mouse.kind,
                    MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                );
            if terminal_mouse_event || selection_drag_event {
                handle_attached_terminal_mouse(app, mouse, terminal_rect);
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
                let Some(doc_y) =
                    switcher_document_y_for_mouse(mouse, terminal_rect, app.switch_scroll)
                else {
                    return false;
                };
                if app.select_row_at_document_y(doc_y).is_some() {
                    handle_switch_action(app, terminal_rect);
                }
            }
            _ => {}
        },
    }
    false
}

fn handle_attached_terminal_mouse(app: &mut App, mouse: MouseEvent, terminal_rect: Rect) {
    /*
     * CDXC:GhostexTui 2026-06-07-16:19:
     * Host-side text selection should start when the attached child has not
     * enabled mouse reporting. Preserve terminal-app mouse behavior by
     * forwarding reported mouse events first, then falling back to selection
     * anchor/drag/copy for ordinary shell and agent output.
     */
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.clear_selection();
            let forwarded = app
                .pty
                .as_mut()
                .is_some_and(|pty| pty.forward_mouse_button(mouse, terminal_rect));
            if !forwarded {
                start_attached_terminal_selection(app, mouse, terminal_rect);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.selection.is_some() {
                drag_attached_terminal_selection(app, mouse, terminal_rect);
            } else if let Some(pty) = app.pty.as_mut() {
                pty.forward_mouse_button(mouse, terminal_rect);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if app.selection.is_some() {
                finish_attached_terminal_selection(app);
            } else if let Some(pty) = app.pty.as_mut() {
                pty.forward_mouse_button(mouse, terminal_rect);
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            if app
                .selection
                .as_ref()
                .is_some_and(|selection| !selection.is_in_progress())
            {
                app.clear_selection();
            }
            if let Some(pty) = app.pty.as_mut() {
                pty.handle_wheel(mouse, terminal_rect);
            }
        }
        _ => {
            app.clear_selection();
            if let Some(pty) = app.pty.as_mut() {
                pty.forward_mouse_button(mouse, terminal_rect);
            }
        }
    }
}

fn start_attached_terminal_selection(app: &mut App, mouse: MouseEvent, terminal_rect: Rect) {
    let Some(pty) = app.pty.as_ref() else {
        return;
    };
    let pane_id = pty.pane_id;
    let metrics = pty.runtime.scroll_metrics();
    app.stop_selection_autoscroll();
    app.selection = Some(attached_terminal_selection_anchor(
        mouse,
        terminal_rect,
        pane_id,
        metrics,
    ));
}

fn drag_attached_terminal_selection(app: &mut App, mouse: MouseEvent, terminal_rect: Rect) {
    let Some(selection) = app.selection.as_ref() else {
        return;
    };
    let metrics = app
        .pty
        .as_ref()
        .and_then(|pty| pty.runtime.scroll_metrics());
    let (screen_col, screen_row) = attached_terminal_selection_screen_pos(mouse);
    let was_dragging = selection.is_dragging();
    let (anchor_screen_row, anchor_screen_col) =
        selection.anchor_screen_pos(terminal_rect, metrics);
    let anchor_differs_from_mouse =
        anchor_screen_row != screen_row || anchor_screen_col != screen_col;
    let is_dragging = was_dragging || anchor_differs_from_mouse;
    if let Some(selection) = app.selection.as_mut() {
        drag_attached_terminal_selection_with_metrics(selection, mouse, terminal_rect, metrics);
        if is_dragging && selection.is_just_click() {
            selection.force_dragging();
        }
    }
    update_attached_selection_autoscroll(app, screen_col, screen_row, terminal_rect, is_dragging);
}

fn finish_attached_terminal_selection(app: &mut App) {
    app.stop_selection_autoscroll();
    let Some(selection) = app.selection.as_mut() else {
        return;
    };
    if !selection.finish() {
        app.clear_selection();
        return;
    }
    let text = app
        .pty
        .as_ref()
        .and_then(|pty| pty.runtime.extract_selection(selection));
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        selection::write_osc52_bytes(text.as_bytes());
    }
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
                        let created_session_key =
                            session_identity_key_parts(project_id.as_deref(), &session_id);
                        if let Some(row_index) = app.row_index_for_session_key(&created_session_key)
                        {
                            app.selected_row_index = row_index;
                            app.sync_selected_session_index_from_row();
                            if let Some(session) = app.session_by_key(&created_session_key).cloned()
                            {
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
        ContextActionKind::SessionFavorite { session, favorite } => run_ghostex_cli(
            &session_command_args("favorite-session", &session, Some(favorite)),
        )
        .map(|_| ()),
        ContextActionKind::SessionSleep { session, sleeping } => run_ghostex_cli(
            &session_command_args("sleep-session", &session, Some(sleeping)),
        )
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
        InputPromptAction::RenameSession(session) => {
            let mut args = session_command_args("rename-session", &session, None);
            args.extend(["--title".to_string(), prompt.value]);
            run_ghostex_cli(&args).map(|_| ())
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
    run_ghostex_cli(&session_command_args(command, session, None)).map(|_| ())
}

fn acknowledge_session_attention(session: &SessionItem) -> io::Result<()> {
    run_ghostex_cli(&session_command_args(
        "acknowledge-session-attention",
        session,
        None,
    ))
    .map(|_| ())
}

fn session_command_args(
    command: &str,
    session: &SessionItem,
    boolean: Option<bool>,
) -> Vec<String> {
    /*
     * CDXC:GxTuiSessions 2026-05-31-08:45:
     * The TUI renders the shared `ghostex sessions --json` inventory and should
     * pass both projectId and sessionId back for gxserver-scoped lifecycle
     * actions. Keep bare session-id fallback in the CLI, but make the TUI's
     * normal path direct like macOS, Android, iOS, and `gx ls`.
     */
    let mut args = vec![
        command.to_string(),
        "--session-id".to_string(),
        session.session_id.clone(),
    ];
    if let Some(project_id) = session
        .project_id
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        args.extend(["--project-id".to_string(), project_id.to_string()]);
    }
    if let Some(value) = boolean {
        args.push(value.to_string());
    }
    args
}

fn run_project_session_command(
    sessions: &[SessionItem],
    command: &str,
    boolean: Option<bool>,
) -> io::Result<()> {
    for session in sessions {
        let args = session_command_args(command, session, boolean);
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
    std::env::var("GHOSTEX_TUI_CLI_COMMAND").unwrap_or_else(|_| "gx".to_string())
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

fn switcher_content_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

fn switcher_document_y_for_mouse(
    mouse: MouseEvent,
    switcher_area: Rect,
    switch_scroll: usize,
) -> Option<usize> {
    /*
     * CDXC:GhostexTui 2026-06-07-16:40:
     * Switcher clicks target rows inside the bordered list, not the outer
     * terminal area. Subtract the block's top border during hit testing so a
     * click lands on the visible row under the pointer instead of the next row.
     */
    let content = switcher_content_rect(switcher_area);
    if !rect_contains(content, mouse.column, mouse.row) {
        return None;
    }
    Some(switch_scroll.saturating_add(mouse.row.saturating_sub(content.y) as usize))
}

fn attached_terminal_mouse_cell(mouse: MouseEvent, terminal_rect: Rect) -> (u16, u16) {
    /*
     * CDXC:GhostexTui 2026-06-07-15:56:
     * Attached Ghostex TUI mouse events arrive one screen row lower than the
     * Ghostty viewport row the user clicked. Normalize the row before Ghostty's
     * mouse encoder adds the terminal protocol's one-based SGR offset so clicks
     * and reported wheel events land on the rendered terminal cell.
     */
    (
        mouse.column.saturating_sub(terminal_rect.x),
        mouse.row.saturating_sub(terminal_rect.y.saturating_add(1)),
    )
}

fn attached_terminal_selection_cell(mouse: MouseEvent, terminal_rect: Rect) -> (u16, u16) {
    /*
     * CDXC:GhostexTui 2026-06-07-16:39:
     * Host-side selection highlights the rendered Ghostex TUI viewport, while
     * child-terminal mouse reporting uses attached_terminal_mouse_cell's row
     * normalization. Keep selection on raw screen rows so the highlight stays
     * on the line under the user's pointer.
     */
    (
        mouse.column.saturating_sub(terminal_rect.x),
        mouse.row.saturating_sub(terminal_rect.y),
    )
}

fn attached_terminal_selection_anchor(
    mouse: MouseEvent,
    terminal_rect: Rect,
    pane_id: layout::PaneId,
    metrics: Option<pane::ScrollMetrics>,
) -> selection::Selection {
    let (column, row) = attached_terminal_selection_cell(mouse, terminal_rect);
    selection::Selection::anchor(pane_id, row, column, metrics)
}

fn drag_attached_terminal_selection_with_metrics(
    selection: &mut selection::Selection,
    mouse: MouseEvent,
    terminal_rect: Rect,
    metrics: Option<pane::ScrollMetrics>,
) {
    let (screen_col, screen_row) = attached_terminal_selection_screen_pos(mouse);
    selection.drag(screen_col, screen_row, terminal_rect, metrics);
}

fn attached_terminal_selection_screen_pos(mouse: MouseEvent) -> (u16, u16) {
    (mouse.column, mouse.row)
}

fn update_attached_selection_autoscroll(
    app: &mut App,
    screen_col: u16,
    screen_row: u16,
    terminal_rect: Rect,
    is_dragging: bool,
) {
    /*
     * CDXC:GhostexTui 2026-06-07-16:41:
     * Attached terminal selection should auto-scroll like Herdr when the user
     * drags to the top or bottom edge. Store the last screen position and let
     * the render loop extend the selection while the pointer remains at an
     * edge, stopping at scrollback boundaries.
     */
    if !is_dragging || terminal_rect.height == 0 {
        app.stop_selection_autoscroll();
        return;
    }
    let top = terminal_rect.y;
    let bottom = terminal_rect
        .y
        .saturating_add(terminal_rect.height.saturating_sub(1));
    let direction = if screen_row <= top {
        Some(AttachedSelectionAutoscrollDirection::Up)
    } else if screen_row >= bottom {
        Some(AttachedSelectionAutoscrollDirection::Down)
    } else {
        None
    };
    let Some(direction) = direction else {
        app.stop_selection_autoscroll();
        return;
    };
    app.selection_autoscroll = Some(AttachedSelectionAutoscroll {
        direction,
        last_mouse_screen_col: screen_col,
        last_mouse_screen_row: screen_row,
        terminal_rect,
    });
    if app.selection_autoscroll_deadline.is_none() {
        app.selection_autoscroll_deadline = Some(Instant::now() + SELECTION_AUTOSCROLL_INTERVAL);
    }
}

fn attached_terminal_key_override_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    /*
     * CDXC:GhostexTui 2026-06-07-15:57:
     * Attached Ghostex TUI sessions should treat Shift+Enter as Ctrl+J.
     * Send LF directly instead of preserving modified Enter through Kitty/CSI-u
     * negotiation so agents and shells receive the same input as Ctrl+J.
     */
    let is_shift_enter = key.code == KeyCode::Enter
        && key.modifiers.contains(KeyModifiers::SHIFT)
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    let is_ctrl_j = matches!(key.code, KeyCode::Char('j'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.intersects(KeyModifiers::ALT);
    if !is_shift_enter && !is_ctrl_j {
        return None;
    }
    match key.kind {
        crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat => {
            Some(vec![b'\n'])
        }
        crossterm::event::KeyEventKind::Release => Some(Vec::new()),
    }
}

fn rect_from_size(size: ratatui::layout::Size) -> Rect {
    Rect::new(0, 0, size.width, size.height)
}

fn switch_button_rect(header: Rect) -> Rect {
    let width = HEADER_CONTROL_WIDTH.min(header.width);
    header_control_rect(header, header.x + header.width.saturating_sub(width), width)
}

fn hotkeys_button_rect(header: Rect) -> Rect {
    let switch_width = HEADER_CONTROL_WIDTH.min(header.width);
    let available_width = header.width.saturating_sub(switch_width);
    let width = 14u16.min(available_width);
    header_control_rect(header, header.x, width)
}

fn project_badge_rect(header: Rect) -> Rect {
    let switch_width = HEADER_CONTROL_WIDTH.min(header.width);
    let available_width = header.width.saturating_sub(switch_width);
    let width = HEADER_CONTROL_WIDTH.min(available_width);
    header_control_rect(header, header.x, width)
}

fn header_control_rect(header: Rect, x: u16, width: u16) -> Rect {
    let height = header.height.saturating_sub(1).min(2);
    Rect::new(
        x,
        header.y + header.height.saturating_sub(height),
        width,
        height,
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

fn project_badge_label(session: Option<&SessionItem>) -> String {
    let label = session
        .map(project_label)
        .unwrap_or_else(|| "Project".to_string());
    format!(
        " {} ",
        fixed_display_prefix(&label, HEADER_PROJECT_LABEL_WIDTH)
    )
}

fn fixed_display_prefix(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used_width = 0usize;
    for ch in text.trim().chars().filter(|ch| !ch.is_control()) {
        let char_width = ch.width().unwrap_or(0);
        if used_width + char_width > width {
            break;
        }
        output.push(ch);
        used_width += char_width;
    }
    while used_width < width {
        output.push(' ');
        used_width += 1;
    }
    output
}

fn agent_indicator(session: &SessionItem) -> &'static str {
    /*
    CDXC:GhostexTuiAgentLabels 2026-06-09-11:53:
    The TUI switcher should recognize every built-in macOS sidebar agent, including hidden restorable agents and common command/display-name aliases, so restored sessions do not collapse to UNK when they come from the desktop app.
    Keep these indicators to three columns because switcher rows reserve a fixed-width agent badge before the session title.
    */
    match normalized_agent(session).as_str() {
        "agy" | "anti-gravity" | "anti-gravity-cli" | "antigravity" | "antigravity-cli" => "AGY",
        "amp" | "amp-cli" => "AMP",
        "claude" | "claude-code" | "claude-work" => "CLD",
        "codebuddy" | "code-buddy" => "CDB",
        "codex" | "codex-cli" | "work-codex" | "open-ai" | "openai" | "openai-codex" => "CDX",
        "copilot" | "github-copilot" => "PLT",
        "cursor" | "cursor-agent" | "cursor-cli" => "CRS",
        "droid" | "factory" | "factory-droid" => "DRD",
        "gemini" => "GEM",
        "grok" | "grok-build" => "GRK",
        "hermes" | "hermes-agent" => "HMS",
        "opencode" | "open-code" => "OPC",
        "pi" | "pi-agent" | "π" => "PIA",
        "qoder" | "qodercli" => "QDR",
        "rovo" | "rovo-dev" | "rovodev" => "RVO",
        "t3" | "t3-code" => "T3C",
        _ => "UNK",
    }
}

fn agent_color(session: &SessionItem) -> Color {
    /*
    CDXC:GhostexTuiAgentColors 2026-06-09-11:29:
    Agent labels in the Ghostex TUI switcher should cover the macOS sidebar catalog, including hidden restorable agents. OpenAI/Codex is blue, Claude is orange, Gemini/Antigravity share periwinkle, Cursor/Copilot/Grok/Amp and hidden agents are white, OpenCode and Factory keep distinct product-tinted accents, and unknown/fallback labels are gray.
    Keep this mapping independent from background colors so agent identity remains legible on selected and unselected gray rows.
    */
    match normalized_agent(session).as_str() {
        "agy" | "anti-gravity" | "anti-gravity-cli" | "antigravity" | "antigravity-cli"
        | "gemini" => TUI_AGENT_PERIWINKLE,
        "claude" | "claude-code" | "claude-work" => TUI_AGENT_CLAUDE_ORANGE,
        "codex" | "codex-cli" | "work-codex" | "open-ai" | "openai" | "openai-codex" => {
            TUI_ACCENT_BLUE
        }
        "droid" | "factory" | "factory-droid" => TUI_AGENT_FACTORY_ORANGE,
        "opencode" | "open-code" => TUI_AGENT_OPENCODE_BLUE,
        "amp" | "amp-cli" | "codebuddy" | "code-buddy" | "copilot" | "cursor" | "cursor-agent"
        | "cursor-cli" | "github-copilot" | "grok" | "grok-build" | "hermes" | "hermes-agent"
        | "qoder" | "qodercli" | "rovo" | "rovo-dev" | "rovodev" => Color::White,
        "pi" | "pi-agent" | "π" => Color::Rgb(200, 255, 98),
        "t3" | "t3-code" => Color::Rgb(255, 106, 243),
        _ => TUI_SUBTLE_TEXT,
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
            selection: None,
            selection_autoscroll: None,
            selection_autoscroll_deadline: None,
            switcher_vertical_nav_repeat: None,
            mode: Mode::Switcher,
            switch_scroll: 0,
            last_refresh: Instant::now(),
            known_attention_session_keys: HashSet::new(),
            has_loaded_session_statuses: true,
            status: String::new(),
        }
    }

    fn buffer_row_text(buffer: &ratatui::buffer::Buffer, row: u16, width: u16) -> String {
        (0..width)
            .map(|x| buffer[(x, row)].symbol())
            .collect::<String>()
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
    fn switcher_vertical_arrow_repeats_are_throttled() {
        let mut app = test_app(Vec::new());
        let start = Instant::now();

        assert!(app.should_handle_switcher_vertical_nav(
            SwitcherVerticalNavKey::Down,
            KeyEventKind::Press,
            start
        ));
        assert!(!app.should_handle_switcher_vertical_nav(
            SwitcherVerticalNavKey::Down,
            KeyEventKind::Repeat,
            start + Duration::from_millis(30)
        ));
        assert!(app.should_handle_switcher_vertical_nav(
            SwitcherVerticalNavKey::Down,
            KeyEventKind::Repeat,
            start + SWITCHER_VERTICAL_NAV_REPEAT_INTERVAL
        ));
    }

    #[test]
    fn switcher_vertical_arrow_release_clears_repeat_gate() {
        let mut app = test_app(Vec::new());
        let start = Instant::now();

        assert!(app.should_handle_switcher_vertical_nav(
            SwitcherVerticalNavKey::Up,
            KeyEventKind::Press,
            start
        ));
        assert!(!app.should_handle_switcher_vertical_nav(
            SwitcherVerticalNavKey::Up,
            KeyEventKind::Release,
            start + Duration::from_millis(5)
        ));
        assert!(app.should_handle_switcher_vertical_nav(
            SwitcherVerticalNavKey::Up,
            KeyEventKind::Press,
            start + Duration::from_millis(10)
        ));
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

    #[test]
    fn header_controls_use_nine_column_bottom_two_row_rects() {
        let header = Rect::new(2, 10, 50, 3);

        assert_eq!(project_badge_rect(header), Rect::new(2, 11, 9, 2));
        assert_eq!(switch_button_rect(header), Rect::new(43, 11, 9, 2));
        assert_eq!(hotkeys_button_rect(header), Rect::new(2, 11, 14, 2));
    }

    #[test]
    fn wrapper_palette_uses_gray_backgrounds_and_blue_accent() {
        assert_eq!(TUI_BG, Color::Rgb(22, 24, 28));
        assert_eq!(TUI_SURFACE, Color::Rgb(34, 39, 46));
        assert_eq!(TUI_SELECTED_BG, Color::Rgb(45, 54, 64));
        assert_eq!(TUI_ACCENT_BLUE, Color::Rgb(88, 166, 255));
    }

    #[test]
    fn agent_colors_cover_macos_sidebar_catalog() {
        let mut session = test_session("alpha", "one");
        let cases = [
            ("t3", "T3C", Color::Rgb(255, 106, 243)),
            ("T3 Code", "T3C", Color::Rgb(255, 106, 243)),
            ("codex", "CDX", TUI_ACCENT_BLUE),
            ("Open AI", "CDX", TUI_ACCENT_BLUE),
            ("openai-codex", "CDX", TUI_ACCENT_BLUE),
            ("claude", "CLD", TUI_AGENT_CLAUDE_ORANGE),
            ("Claude Work", "CLD", TUI_AGENT_CLAUDE_ORANGE),
            ("cursor", "CRS", Color::White),
            ("cursor-agent", "CRS", Color::White),
            ("cursor-cli", "CRS", Color::White),
            ("pi", "PIA", Color::Rgb(200, 255, 98)),
            ("Pi Agent", "PIA", Color::Rgb(200, 255, 98)),
            ("opencode", "OPC", TUI_AGENT_OPENCODE_BLUE),
            ("Open Code", "OPC", TUI_AGENT_OPENCODE_BLUE),
            ("gemini", "GEM", TUI_AGENT_PERIWINKLE),
            ("copilot", "PLT", Color::White),
            ("GitHub Copilot", "PLT", Color::White),
            ("droid", "DRD", TUI_AGENT_FACTORY_ORANGE),
            ("Factory Droid", "DRD", TUI_AGENT_FACTORY_ORANGE),
            ("grok", "GRK", Color::White),
            ("Grok Build", "GRK", Color::White),
            ("agy", "AGY", TUI_AGENT_PERIWINKLE),
            ("Antigravity CLI", "AGY", TUI_AGENT_PERIWINKLE),
            ("Anti Gravity", "AGY", TUI_AGENT_PERIWINKLE),
            ("amp", "AMP", Color::White),
            ("Amp CLI", "AMP", Color::White),
            ("rovodev", "RVO", Color::White),
            ("Rovo Dev", "RVO", Color::White),
            ("hermes", "HMS", Color::White),
            ("Hermes Agent", "HMS", Color::White),
            ("codebuddy", "CDB", Color::White),
            ("Code Buddy", "CDB", Color::White),
            ("qoder", "QDR", Color::White),
            ("qodercli", "QDR", Color::White),
            ("unknown-agent", "UNK", TUI_SUBTLE_TEXT),
        ];

        for (agent, indicator, color) in cases {
            session.agent = Some(agent.to_string());
            assert_eq!(
                agent_indicator(&session),
                indicator,
                "indicator for {agent}"
            );
            assert_eq!(agent_color(&session), color, "color for {agent}");
        }
    }

    #[test]
    fn switcher_header_renders_help_hotkeys_and_quit_ghostex_labels() {
        let app = test_app(Vec::new());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(48, 3)).unwrap();

        terminal
            .draw(|frame| render_header(frame, &app, Rect::new(0, 0, 48, 3)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let first_label_row = buffer_row_text(buffer, 1, 48);
        let second_label_row = buffer_row_text(buffer, 2, 48);

        assert!(first_label_row.contains("Help &"));
        assert!(second_label_row.contains("Hotkeys"));
        assert!(first_label_row.contains("Quit"));
        assert!(second_label_row.contains("Ghostex"));
        assert!(!second_label_row.contains("GTX TUI"));
    }

    #[test]
    fn attached_terminal_mouse_cell_removes_attached_row_offset() {
        let terminal = Rect::new(4, 6, 80, 20);
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 14,
            row: 8,
            modifiers: KeyModifiers::empty(),
        };

        assert_eq!(attached_terminal_mouse_cell(mouse, terminal), (10, 1));

        let top_row_mouse = MouseEvent { row: 6, ..mouse };
        assert_eq!(
            attached_terminal_mouse_cell(top_row_mouse, terminal),
            (10, 0)
        );
    }

    #[test]
    fn switcher_document_y_uses_bordered_list_inner_row() {
        let switcher = Rect::new(0, 0, 80, 10);
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 3,
            modifiers: KeyModifiers::empty(),
        };

        assert_eq!(switcher_document_y_for_mouse(mouse, switcher, 10), Some(12));

        let top_border = MouseEvent { row: 0, ..mouse };
        assert_eq!(
            switcher_document_y_for_mouse(top_border, switcher, 10),
            None
        );
    }

    #[test]
    fn attached_terminal_selection_uses_rendered_screen_rows() {
        let terminal = Rect::new(4, 6, 80, 20);
        let pane_id = layout::PaneId::from_raw(42);
        let anchor = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 14,
            row: 8,
            modifiers: KeyModifiers::empty(),
        };
        let mut selection = attached_terminal_selection_anchor(anchor, terminal, pane_id, None);

        assert!(!selection.is_visible());

        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 16,
            row: 10,
            modifiers: KeyModifiers::empty(),
        };
        drag_attached_terminal_selection_with_metrics(&mut selection, drag, terminal, None);

        assert!(selection.is_visible());
        assert!(selection.contains(2, 10, None));
        assert!(selection.contains(4, 12, None));
        assert!(!selection.contains(5, 12, None));
    }

    #[test]
    fn attached_selection_autoscroll_arms_on_terminal_edges() {
        let mut app = test_app(Vec::new());
        let terminal = Rect::new(0, 2, 80, 10);

        update_attached_selection_autoscroll(&mut app, 8, 2, terminal, true);

        assert_eq!(
            app.selection_autoscroll.map(|state| state.direction),
            Some(AttachedSelectionAutoscrollDirection::Up)
        );

        update_attached_selection_autoscroll(&mut app, 8, 6, terminal, true);
        assert!(app.selection_autoscroll.is_none());

        update_attached_selection_autoscroll(&mut app, 8, 11, terminal, true);
        assert_eq!(
            app.selection_autoscroll.map(|state| state.direction),
            Some(AttachedSelectionAutoscrollDirection::Down)
        );
    }

    #[test]
    fn attached_terminal_shift_enter_sends_ctrl_j_bytes() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);

        assert_eq!(attached_terminal_key_override_bytes(key), Some(vec![b'\n']));
    }

    #[test]
    fn attached_terminal_shift_enter_release_sends_no_bytes() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
            crossterm::event::KeyEventKind::Release,
        );

        assert_eq!(attached_terminal_key_override_bytes(key), Some(Vec::new()));
    }

    #[test]
    fn attached_terminal_plain_enter_uses_runtime_encoder() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());

        assert_eq!(attached_terminal_key_override_bytes(key), None);
    }

    #[test]
    fn attached_terminal_ctrl_j_sends_lf_bytes() {
        let key = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);

        assert_eq!(attached_terminal_key_override_bytes(key), Some(vec![b'\n']));
    }

    #[test]
    fn project_badge_label_uses_first_seven_project_columns() {
        let mut session = test_session("alpha", "one");
        session.project_name = Some("ZmuxProject".to_string());

        assert_eq!(project_badge_label(Some(&session)), " ZmuxPro ");
    }

    #[test]
    fn activity_count_badge_stays_inside_nine_columns() {
        let text = activity_count_badge_spans(999, 999, Color::Reset)
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, "●999 ●999");
        assert!(text.chars().count() <= HEADER_CONTROL_WIDTH as usize);
    }

    #[test]
    fn attached_header_renders_project_badge_counts_and_session_title() {
        let active = SessionItem {
            activity: Some("working".to_string()),
            project_name: Some("ZmuxProject".to_string()),
            title: "Terminal".to_string(),
            ..test_session("alpha", "one")
        };
        let attention = SessionItem {
            activity: Some("attention".to_string()),
            ..test_session("alpha", "attention")
        };
        let mut app = test_app(vec![ProjectGroup {
            project_id: Some("alpha".to_string()),
            group_id: Some("alpha-group".to_string()),
            name: "ZmuxProject".to_string(),
            path: Some("/alpha".to_string()),
            sessions: vec![active.clone(), attention],
        }]);
        app.mode = Mode::Attached;
        app.active_session = Some(active);

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(48, 3)).unwrap();
        terminal
            .draw(|frame| render_header(frame, &app, Rect::new(0, 0, 48, 3)))
            .unwrap();
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer_row_text(buffer, 1, 9), " ZmuxPro ");
        assert_eq!(buffer_row_text(buffer, 2, 9), " ● 1 ● 1 ");
        let title_row = buffer_row_text(buffer, 1, 48);
        assert!(title_row.contains("Terminal"));
        assert!(!title_row.contains("Ghostex"));
    }

    #[test]
    fn attach_shell_command_includes_project_id_when_available() {
        let mut session = test_session("P1aa", "one");
        session.session_id = "G1aa".to_string();

        assert_eq!(
            attach_shell_command_with_cli("gx", &session),
            "gx attach --session-id 'G1aa' --project-id 'P1aa'"
        );
    }

    #[test]
    fn duplicate_session_ids_match_by_project_scoped_key() {
        let mut first = test_session("P1aa", "one");
        first.session_id = "Gsame".to_string();
        let mut second = test_session("P2bb", "two");
        second.session_id = "Gsame".to_string();

        let app = test_app(vec![
            ProjectGroup {
                project_id: first.project_id.clone(),
                group_id: first.group_id.clone(),
                name: "P1aa".to_string(),
                path: Some("/P1aa".to_string()),
                sessions: vec![first.clone()],
            },
            ProjectGroup {
                project_id: second.project_id.clone(),
                group_id: second.group_id.clone(),
                name: "P2bb".to_string(),
                path: Some("/P2bb".to_string()),
                sessions: vec![second.clone()],
            },
        ]);

        let first_key = session_identity_key(&first);
        let second_key = session_identity_key(&second);

        assert_ne!(first_key, second_key);
        assert_eq!(app.row_index_for_session_key(&first_key), Some(2));
        assert_eq!(app.row_index_for_session_key(&second_key), Some(5));
        assert_eq!(
            app.session_by_key(&second_key)
                .map(|session| session.project_id.as_deref()),
            Some(Some("P2bb"))
        );
    }
}
