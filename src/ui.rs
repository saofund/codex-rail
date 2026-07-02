use crate::attach;
use crate::state::{
    self, SessionState, STATUS_EXITED, STATUS_FAILED, STATUS_RUNNING, STATUS_STARTING,
    STATUS_STOPPING,
};
use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use std::env;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STOP_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const EXIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const REFRESH_INTERVAL: Duration = Duration::from_millis(700);

pub fn run_manager() -> Result<()> {
    state::ensure_base_dirs()?;
    let mut terminal = TerminalSession::enter()?;
    let mut app = App::load()?;
    let result = manager_loop(&mut terminal, &mut app);
    terminal.leave().ok();
    result
}

fn manager_loop(terminal: &mut TerminalSession, app: &mut App) -> Result<()> {
    let mut last_refresh = Instant::now();
    render(app)?;

    loop {
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.reload()?;
            last_refresh = Instant::now();
            render(app)?;
        }

        if !event::poll(Duration::from_millis(80))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if handle_key(key, app, terminal)? {
                    return Ok(());
                }
                render(app)?;
            }
            Event::Mouse(mouse) => {
                if handle_mouse(mouse.kind, mouse.row, app, terminal)? {
                    return Ok(());
                }
                render(app)?;
            }
            Event::Resize(_, _) => render(app)?,
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    New,
    Rename,
}

struct App {
    sessions: Vec<SessionState>,
    selected: usize,
    mode: Mode,
    input: String,
    message: String,
    stop_confirm: Option<(String, Instant)>,
    exit_confirm: Option<Instant>,
    rows: Vec<(u16, usize)>,
}

impl App {
    fn load() -> Result<Self> {
        let sessions = state::load_sessions()?;
        Ok(Self {
            sessions,
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            message: String::new(),
            stop_confirm: None,
            exit_confirm: None,
            rows: Vec::new(),
        })
    }

    fn reload(&mut self) -> Result<()> {
        let selected_id = self.current().map(|s| s.id.clone());
        self.sessions = state::load_sessions()?;
        if let Some(id) = selected_id {
            if let Some(pos) = self.sessions.iter().position(|s| s.id == id) {
                self.selected = pos;
            }
        }
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
        Ok(())
    }

    fn current(&self) -> Option<&SessionState> {
        self.sessions.get(self.selected)
    }

    fn move_prev(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
        self.clear_transient();
    }

    fn move_next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = (self.selected + 1).min(self.sessions.len() - 1);
        self.clear_transient();
    }

    fn clear_transient(&mut self) {
        self.stop_confirm = None;
        self.exit_confirm = None;
        if self.mode == Mode::Normal {
            self.message.clear();
        }
    }
}

fn handle_key(key: KeyEvent, app: &mut App, terminal: &mut TerminalSession) -> Result<bool> {
    match app.mode {
        Mode::Normal => handle_normal_key(key, app, terminal),
        Mode::New => handle_input_key(key, app, terminal, Mode::New),
        Mode::Rename => handle_input_key(key, app, terminal, Mode::Rename),
    }
}

fn handle_normal_key(key: KeyEvent, app: &mut App, terminal: &mut TerminalSession) -> Result<bool> {
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('r') {
        if let Some(session) = app.current() {
            app.input = session.title.clone();
            app.mode = Mode::Rename;
            app.stop_confirm = None;
            app.exit_confirm = None;
            app.message = "rename session".to_string();
        }
        return Ok(false);
    }

    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('x') {
        stop_with_confirmation(app)?;
        return Ok(false);
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('w') if key.modifiers.is_empty() => app.move_prev(),
        KeyCode::Down | KeyCode::Char('s') if key.modifiers.is_empty() => app.move_next(),
        KeyCode::Right | KeyCode::Enter | KeyCode::Char('d') if key.modifiers.is_empty() => {
            attach_current(app, terminal)?;
        }
        KeyCode::Esc => {
            if confirm_exit(app) {
                return Ok(true);
            }
        }
        KeyCode::Left => {
            app.stop_confirm = None;
            app.exit_confirm = None;
            app.message.clear();
        }
        KeyCode::Char('e') if key.modifiers.is_empty() => {
            app.mode = Mode::New;
            app.input.clear();
            app.stop_confirm = None;
            app.exit_confirm = None;
            app.message = "new session".to_string();
        }
        KeyCode::Char(ch) if key.modifiers.is_empty() && !ch.is_control() => {
            app.mode = Mode::New;
            app.input.clear();
            app.input.push(ch);
            app.stop_confirm = None;
            app.exit_confirm = None;
            app.message = "new session".to_string();
        }
        _ => {}
    }

    Ok(false)
}

fn handle_input_key(
    key: KeyEvent,
    app: &mut App,
    terminal: &mut TerminalSession,
    mode: Mode,
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.input.clear();
            app.stop_confirm = None;
            app.exit_confirm = None;
            app.message.clear();
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Enter => {
            submit_input(app, terminal, mode)?;
        }
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
            submit_input(app, terminal, mode)?;
        }
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            if !ch.is_control() {
                app.input.push(ch);
            }
        }
        _ => {}
    }
    Ok(false)
}

fn submit_input(app: &mut App, terminal: &mut TerminalSession, mode: Mode) -> Result<()> {
    let text = app.input.trim().to_string();
    if text.is_empty() {
        app.message = "empty name ignored".to_string();
        return Ok(());
    }

    match mode {
        Mode::New => match create_session(&text) {
            Ok(session) => {
                app.mode = Mode::Normal;
                app.input.clear();
                app.reload()?;
                if let Some(pos) = app.sessions.iter().position(|s| s.id == session.id) {
                    app.selected = pos;
                }
                attach_current(app, terminal)?;
            }
            Err(err) => {
                app.message = format!("create failed: {err:#}");
            }
        },
        Mode::Rename => {
            if let Some(current) = app.current() {
                match state::read_state(&current.id).and_then(|mut session| {
                    session.title = text;
                    session.updated_at = state::now_secs();
                    state::write_state(&session)
                }) {
                    Ok(()) => {
                        app.mode = Mode::Normal;
                        app.input.clear();
                        app.message = "renamed".to_string();
                        app.reload()?;
                    }
                    Err(err) => {
                        app.message = format!("rename failed: {err:#}");
                    }
                }
            }
        }
        Mode::Normal => {}
    }
    Ok(())
}

fn handle_mouse(
    kind: MouseEventKind,
    row: u16,
    app: &mut App,
    terminal: &mut TerminalSession,
) -> Result<bool> {
    let row_index = app
        .rows
        .iter()
        .find_map(|(known_row, index)| (*known_row == row).then_some(*index));

    match kind {
        MouseEventKind::Moved => {
            if let Some(index) = row_index {
                app.selected = index;
                app.clear_transient();
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(index) = row_index {
                app.selected = index;
                attach_current(app, terminal)?;
            }
        }
        _ => {}
    }
    Ok(false)
}

fn attach_current(app: &mut App, terminal: &mut TerminalSession) -> Result<()> {
    let Some(session) = app.current().cloned() else {
        app.message = "no session".to_string();
        return Ok(());
    };

    terminal.leave()?;
    let result = attach::attach_session(&session);
    terminal.enter_again()?;
    app.reload()?;

    match result {
        Ok(()) => {
            app.message = "detached".to_string();
            Ok(())
        }
        Err(err) => {
            app.message = format!("attach failed: {err:#}");
            Ok(())
        }
    }
}

fn create_session(title: &str) -> Result<SessionState> {
    state::ensure_base_dirs()?;
    let id = state::new_session_id();
    let cwd = env::current_dir().context("current directory")?;
    let codex = env::var("CODEX_RAIL_CODEX").unwrap_or_else(|_| "codex".to_string());
    let socket = state::socket_path(&id);
    let now = state::now_secs();

    let mut session = SessionState {
        id: id.clone(),
        title: title.to_string(),
        cwd: cwd.to_string_lossy().to_string(),
        codex,
        status: STATUS_STARTING.to_string(),
        worker_pid: None,
        child_pid: None,
        socket: socket.to_string_lossy().to_string(),
        created_at: now,
        updated_at: now,
        exit_code: None,
        last_error: None,
    };
    state::write_state(&session)?;

    let mut child = Command::new(env::current_exe().context("current executable")?)
        .arg("--worker")
        .arg(&id)
        .current_dir(Path::new(&session.cwd))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn worker for {title}"))?;

    // The worker writes its own worker_pid/status once it has bound the
    // socket, so this is just for the value returned below; don't persist
    // it here or a slow scheduler could let this stale (still "starting")
    // copy clobber the worker's own "running" write.
    session.worker_pid = Some(child.id());

    // Reap the worker when it exits instead of leaking a zombie under the
    // manager process for as long as the manager stays open.
    thread::spawn(move || {
        let _ = child.wait();
    });

    wait_for_socket(&session.socket);
    Ok(session)
}

fn wait_for_socket(socket: &str) {
    let path = Path::new(socket);
    for _ in 0..30 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn stop_with_confirmation(app: &mut App) -> Result<()> {
    let Some(session) = app.current().cloned() else {
        app.message = "no session".to_string();
        return Ok(());
    };

    let confirmed = app
        .stop_confirm
        .as_ref()
        .map(|(id, at)| id == &session.id && at.elapsed() <= STOP_CONFIRM_WINDOW)
        .unwrap_or(false);

    if !confirmed {
        app.stop_confirm = Some((session.id, Instant::now()));
        app.message = "Ctrl-X again to stop this session".to_string();
        return Ok(());
    }

    match UnixStream::connect(&session.socket) {
        Ok(mut stream) => {
            match stream.write_all(b"STOP\n").and_then(|_| stream.flush()) {
                Ok(()) => {
                    app.message = "stop requested".to_string();
                    app.reload()?;
                }
                Err(err) => {
                    app.message = format!("stop failed: {err}");
                }
            }
            app.stop_confirm = None;
        }
        Err(err) => {
            app.stop_confirm = None;
            app.message = format!("stop failed: {err}");
        }
    }
    Ok(())
}

fn confirm_exit(app: &mut App) -> bool {
    let confirmed = app
        .exit_confirm
        .map(|at| at.elapsed() <= EXIT_CONFIRM_WINDOW)
        .unwrap_or(false);
    if confirmed {
        true
    } else {
        app.exit_confirm = Some(Instant::now());
        app.stop_confirm = None;
        app.message = "Esc again to leave manager".to_string();
        false
    }
}

fn render(app: &mut App) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((100, 30));
    let mut stdout = io::stdout();
    queue!(stdout, ResetColor, Hide, Clear(ClearType::All), MoveTo(0, 0))?;

    draw_header(&mut stdout, cols)?;
    draw_sessions(&mut stdout, app, cols, rows)?;
    draw_input(&mut stdout, app, cols, rows)?;

    stdout.flush()?;
    Ok(())
}

fn draw_header(stdout: &mut io::Stdout, cols: u16) -> Result<()> {
    queue!(
        stdout,
        MoveTo(0, 0),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print(fit("Codex Rail", cols as usize)),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    queue!(
        stdout,
        MoveTo(0, 1),
        SetForegroundColor(Color::Grey),
        Print(fit("w/s or arrows move, d/right/enter attach, e new, Ctrl-R rename, Ctrl-X Ctrl-X stop, Esc Esc exit", cols as usize)),
        ResetColor
    )?;
    Ok(())
}

fn draw_sessions(stdout: &mut io::Stdout, app: &mut App, cols: u16, rows: u16) -> Result<()> {
    app.rows.clear();
    let start_y = 3_u16;
    let bottom_reserved = 5_u16;
    let max_rows = rows.saturating_sub(start_y + bottom_reserved) as usize;

    if app.sessions.is_empty() {
        queue!(
            stdout,
            MoveTo(0, start_y),
            SetForegroundColor(Color::Grey),
            Print(fit(
                "No sessions. Press e, type a name, Enter.",
                cols as usize
            )),
            ResetColor
        )?;
        return Ok(());
    }

    let offset = if max_rows == 0 {
        0
    } else {
        app.selected.saturating_sub(max_rows.saturating_sub(1))
    };

    for (visible, (index, session)) in app
        .sessions
        .iter()
        .enumerate()
        .skip(offset)
        .take(max_rows)
        .enumerate()
    {
        let y = start_y + visible as u16;
        app.rows.push((y, index));
        let selected = index == app.selected;
        if selected {
            queue!(
                stdout,
                SetBackgroundColor(Color::DarkGrey),
                SetForegroundColor(Color::White)
            )?;
        } else {
            queue!(
                stdout,
                ResetColor,
                SetForegroundColor(Color::Grey)
            )?;
        }

        let marker = if selected { ">" } else { " " };
        let line = format!(
            "{} {:<10} {:<24} {}",
            marker,
            status_label(&session.status),
            truncate(&session.title, 24),
            truncate(&session.cwd, cols.saturating_sub(40) as usize),
        );
        queue!(
            stdout,
            MoveTo(0, y),
            Print(fit(&line, cols as usize)),
            ResetColor
        )?;
    }

    Ok(())
}

fn draw_input(stdout: &mut io::Stdout, app: &App, cols: u16, rows: u16) -> Result<()> {
    let box_y = rows.saturating_sub(4);
    let width = cols as usize;
    let top = format!("+{}+", "-".repeat(width.saturating_sub(2)));
    queue!(
        stdout,
        ResetColor,
        SetForegroundColor(Color::Grey),
        MoveTo(0, box_y),
        Print(fit(&top, width)),
        ResetColor
    )?;

    let prompt = match app.mode {
        Mode::Normal => "new> ",
        Mode::New => "new> ",
        Mode::Rename => "rename> ",
    };
    let body_text = match app.mode {
        Mode::Normal => "press e or start typing".to_string(),
        Mode::New | Mode::Rename => app.input.clone(),
    };
    let body = format!("| {}{}", prompt, body_text);
    queue!(stdout, MoveTo(0, box_y + 1))?;
    if app.mode == Mode::Normal {
        queue!(stdout, SetForegroundColor(Color::Grey))?;
    } else {
        queue!(stdout, SetForegroundColor(Color::White))?;
    }
    queue!(stdout, Print(fit_with_border(&body, width)), ResetColor)?;

    let bottom = format!("+{}+", "-".repeat(width.saturating_sub(2)));
    queue!(
        stdout,
        SetForegroundColor(Color::Grey),
        MoveTo(0, box_y + 2),
        Print(fit(&bottom, width)),
        ResetColor
    )?;

    queue!(
        stdout,
        MoveTo(0, box_y + 3),
        SetForegroundColor(Color::Yellow),
        Print(fit(&app.message, width)),
        ResetColor
    )?;
    Ok(())
}

fn status_label(status: &str) -> &'static str {
    match status {
        STATUS_RUNNING => "running",
        STATUS_STARTING => "starting",
        STATUS_STOPPING => "stopping",
        STATUS_EXITED => "exited",
        STATUS_FAILED => "failed",
        _ => "unknown",
    }
}

fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars() {
        if out.len() + ch.len_utf8() > max.saturating_sub(1) {
            out.push('~');
            return out;
        }
        out.push(ch);
    }
    out
}

fn fit(text: &str, width: usize) -> String {
    let mut out = truncate(text, width);
    let pad = width.saturating_sub(out.len());
    out.push_str(&" ".repeat(pad));
    out
}

fn fit_with_border(text: &str, width: usize) -> String {
    if width <= 1 {
        return fit(text, width);
    }
    let mut out = truncate(text, width - 1);
    let pad = width.saturating_sub(out.len() + 1);
    out.push_str(&" ".repeat(pad));
    out.push('|');
    out
}

struct TerminalSession {
    active: bool,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        let mut session = Self { active: false };
        session.enter_again()?;
        Ok(session)
    }

    fn enter_again(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        terminal::enable_raw_mode().context("enable raw mode")?;
        if let Err(err) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide) {
            terminal::disable_raw_mode().ok();
            return Err(err.into());
        }
        self.active = true;
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let screen_result = execute!(
            io::stdout(),
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let raw_result = terminal::disable_raw_mode().context("disable raw mode");
        self.active = false;
        screen_result?;
        raw_result?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.leave().ok();
    }
}
