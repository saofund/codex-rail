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
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STOP_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const EXIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const REFRESH_INTERVAL: Duration = Duration::from_millis(700);
const ACTIVITY_IDLE_THRESHOLD_SECS: u64 = 6;
const ROLLOUT_TAIL_BYTES: u64 = 64 * 1024;

// Truecolor palette, tuned for dark terminals. Kept as RGB rather than the
// 8 legacy ANSI colors so it looks consistent regardless of the user's
// terminal theme (the old DarkGrey rendered as near-invisible "black dots").
//
// The brand accent is a warm "terracotta", matching Claude Code's own chrome
// so rail reads as part of the same toolset. Status colours (amber/green/grey)
// stay distinct from the brand accent so they still carry meaning.
const C_ACCENT: Color = Color::Rgb { r: 214, g: 122, b: 90 }; // terracotta: title, prompt, selection bar
const C_ACCENT_DIM: Color = Color::Rgb { r: 150, g: 92, b: 74 }; // dim terracotta: input border
const C_TITLE: Color = Color::Rgb { r: 230, g: 230, b: 238 }; // primary text
const C_SELTITLE: Color = Color::Rgb { r: 255, g: 255, b: 255 }; // selected primary
const C_DIM: Color = Color::Rgb { r: 132, g: 140, b: 158 }; // secondary (paths, age, hints)
const C_FAINT: Color = Color::Rgb { r: 96, g: 102, b: 120 }; // rules
const C_SEL_BG: Color = Color::Rgb { r: 42, g: 38, b: 46 }; // selected row background (warm-tinted)
const C_NEEDS: Color = Color::Rgb { r: 236, g: 188, b: 92 }; // amber
const C_WORKING: Color = Color::Rgb { r: 122, g: 208, b: 142 }; // green
const C_STOPPED: Color = Color::Rgb { r: 140, g: 148, b: 168 }; // grey (readable, not "black")

// Cap the content region so age/metadata sit in a tidy column instead of
// flying to the far right edge of a wide terminal (that left a dead gap in
// the middle of every row). Everything past this is intentional margin.
const MAX_CONTENT_COLS: usize = 78;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activity {
    Active,
    Waiting,
}

fn classify_activity(session: &SessionState) -> Activity {
    // No usable rollout signal (path never captured, or the file vanished)
    // means there's no turn in flight we can see — treat it as waiting for you
    // rather than trusting PTY-output timing, which codex's always-animating
    // TUI keeps "fresh" even while idle (that was the old stuck-on-Working bug).
    classify_from_rollout(session).unwrap_or(Activity::Waiting)
}

// Best-effort: tail the codex rollout JSONL and read the turn lifecycle.
// codex writes an `event_msg` with `payload.type = "task_started"` when it
// begins working on a turn and `"task_complete"` when it finishes and is
// waiting for the user. Scanning newest-to-oldest, whichever marker we hit
// first is the current state. Everything here depends on an undocumented,
// reverse-engineered codex-cli format (verified against cli_version 0.142.5),
// so any parse failure just falls through to PTY timing instead.
fn classify_from_rollout(session: &SessionState) -> Option<Activity> {
    let path = session.codex_rollout_path.as_ref()?;
    let path = Path::new(path);
    let tail = read_tail(path)?;
    for line in tail.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event_type = value
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(|t| t.as_str());
        match event_type {
            Some("task_complete") => return Some(Activity::Waiting),
            Some("task_started") => return Some(Activity::Active),
            _ => {}
        }
    }
    // No lifecycle marker in the tail window — a very long turn can push
    // task_started past it. Fall back to the rollout's mtime: codex appends
    // events continuously while it works but writes nothing while idle at the
    // prompt, so a recently-grown file means Active. Immune to idle TUI redraws.
    let modified_age = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX);
    Some(if modified_age <= ACTIVITY_IDLE_THRESHOLD_SECS {
        Activity::Active
    } else {
        Activity::Waiting
    })
}

fn read_tail(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(ROLLOUT_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    // from_utf8_lossy tolerates a possibly-truncated leading byte sequence
    // from the seek; the first (partial) line is scanned last and skipped if
    // it fails to parse.
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// Sessions are grouped into these buckets in the list, mirroring Claude
// Code's own agents panel (Needs input / Working / Stopped).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bucket {
    NeedsInput,
    Working,
    Stopped,
}

fn session_bucket(s: &SessionState) -> Bucket {
    match s.status.as_str() {
        STATUS_RUNNING => match classify_activity(s) {
            Activity::Waiting => Bucket::NeedsInput,
            Activity::Active => Bucket::Working,
        },
        STATUS_STARTING | STATUS_STOPPING => Bucket::Working,
        _ => Bucket::Stopped, // exited, failed, unknown
    }
}

fn bucket_rank(b: Bucket) -> u8 {
    match b {
        Bucket::NeedsInput => 0, // things needing your attention float to the top
        Bucket::Working => 1,
        Bucket::Stopped => 2,
    }
}

// Fixed slot index for each bucket, used to always lay the sections out in the
// same order (Needs input / Working / Stopped) whether or not they're empty.
fn bucket_slot(b: Bucket) -> usize {
    match b {
        Bucket::NeedsInput => 0,
        Bucket::Working => 1,
        Bucket::Stopped => 2,
    }
}

fn bucket_title(b: Bucket) -> &'static str {
    match b {
        Bucket::NeedsInput => "Needs input",
        Bucket::Working => "Working",
        Bucket::Stopped => "Stopped",
    }
}

fn bucket_color(b: Bucket) -> Color {
    match b {
        Bucket::NeedsInput => C_NEEDS,
        Bucket::Working => C_WORKING,
        Bucket::Stopped => C_STOPPED,
    }
}

fn bucket_glyph(b: Bucket) -> char {
    match b {
        Bucket::NeedsInput => '●', // filled amber: wants your attention
        Bucket::Working => '✻',    // busy
        Bucket::Stopped => '○',    // hollow: inactive/done
    }
}

fn last_activity_secs(s: &SessionState) -> u64 {
    let base = s.updated_at.max(s.last_output_at);
    state::now_secs().saturating_sub(base)
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

// Terminal display width of a char: CJK/fullwidth/most emoji occupy 2 cells.
// Approximate (no unicode-width dependency) but covers the ranges that show
// up in session titles so CJK columns line up instead of drifting right.
fn char_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x20 {
        return 0;
    }
    let wide = (0x1100..=0x115F).contains(&u) // Hangul Jamo
        || (0x2E80..=0x303E).contains(&u)      // CJK radicals / Kangxi / symbols
        || (0x3041..=0x33FF).contains(&u)      // Hiragana..CJK compat
        || (0x3400..=0x4DBF).contains(&u)      // CJK Ext A
        || (0x4E00..=0x9FFF).contains(&u)      // CJK Unified
        || (0xA000..=0xA4CF).contains(&u)      // Yi
        || (0xAC00..=0xD7A3).contains(&u)      // Hangul syllables
        || (0xF900..=0xFAFF).contains(&u)      // CJK compat ideographs
        || (0xFE30..=0xFE4F).contains(&u)      // CJK compat forms
        || (0xFF00..=0xFF60).contains(&u)      // Fullwidth forms
        || (0xFFE0..=0xFFE6).contains(&u)      // Fullwidth signs
        || (0x1F300..=0x1FAFF).contains(&u)    // emoji / symbols
        || (0x20000..=0x3FFFD).contains(&u); // CJK Ext B+
    if wide {
        2
    } else {
        1
    }
}

fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

// Truncate to at most `width` display columns and pad with spaces to exactly
// `width` columns (CJK-aware), so every column lines up.
fn fit_cols(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    while w < width {
        out.push(' ');
        w += 1;
    }
    out
}

// Like fit_cols but keeps the TAIL (with a leading ellipsis), so a long path
// shows the meaningful project dir at the end rather than the root prefix.
fn fit_cols_tail(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return fit_cols(s, width);
    }
    if width == 0 {
        return String::new();
    }
    let mut tail = String::new();
    let mut w = 0;
    for c in s.chars().rev() {
        let cw = char_width(c);
        if w + cw > width.saturating_sub(1) {
            break;
        }
        tail.push(c);
        w += cw;
    }
    let tail: String = tail.chars().rev().collect();
    fit_cols(&format!("…{tail}"), width)
}

fn home_tilde(cwd: &str) -> String {
    match env::var("HOME") {
        Ok(home) if !home.is_empty() && cwd.starts_with(&home) => {
            format!("~{}", &cwd[home.len()..])
        }
        _ => cwd.to_string(),
    }
}

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
        let mut app = Self {
            sessions: state::load_sessions()?,
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            message: String::new(),
            stop_confirm: None,
            exit_confirm: None,
            rows: Vec::new(),
        };
        app.sort_for_display();
        Ok(app)
    }

    fn reload(&mut self) -> Result<()> {
        let selected_id = self.current().map(|s| s.id.clone());
        self.sessions = state::load_sessions()?;
        sync_titles_from_history(&mut self.sessions);
        self.sort_for_display();
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

    // Order sessions by bucket (Needs input, then Working, then Stopped),
    // most-recently-active first within each. Selection is tracked by id
    // across reloads, so re-sorting doesn't move the cursor off a session.
    fn sort_for_display(&mut self) {
        let mut decorated: Vec<(u8, u64, u64, SessionState)> = self
            .sessions
            .drain(..)
            .map(|s| (bucket_rank(session_bucket(&s)), s.updated_at, s.created_at, s))
            .collect();
        decorated.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| b.2.cmp(&a.2))
        });
        self.sessions = decorated.into_iter().map(|(_, _, _, s)| s).collect();
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
            app.message = "first message · Enter to start · empty = blank".to_string();
        }
        // SPACE is reserved for the upcoming auto-reply toggle. Swallow it here
        // so it doesn't fall through to the "type any key to start a new
        // session" arm below (a stray space shouldn't open the composer).
        KeyCode::Char(' ') if key.modifiers.is_empty() => {}
        KeyCode::Char(ch) if key.modifiers.is_empty() && !ch.is_control() => {
            app.mode = Mode::New;
            app.input.clear();
            app.input.push(ch);
            app.stop_confirm = None;
            app.exit_confirm = None;
            app.message = "first message · Enter to start · empty = blank".to_string();
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

    match mode {
        Mode::New => {
            // Empty input → a plain codex session with an auto-numbered title.
            // Any text → that text is both the list title AND codex's first
            // message, started immediately on spawn (so the rollout, and an
            // accurate status, show up within a second or two).
            let (title, prompt) = if text.is_empty() {
                (default_session_title(&app.sessions), None)
            } else {
                (text.clone(), Some(text.clone()))
            };
            match create_session(&title, prompt) {
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
            }
        }
        Mode::Rename => {
            if text.is_empty() {
                app.message = "empty name ignored".to_string();
                return Ok(());
            }
            if let Some(current) = app.current() {
                match state::read_state(&current.id).and_then(|mut session| {
                    session.title = text;
                    session.title_pinned = true;
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
    let Some(mut session) = app.current().cloned() else {
        app.message = "no session".to_string();
        return Ok(());
    };

    if matches!(session.status.as_str(), STATUS_EXITED | STATUS_FAILED) {
        if let Err(err) = relaunch_worker(&session) {
            app.message = format!("resume failed: {err:#}");
            return Ok(());
        }
        session = match state::read_state(&session.id) {
            Ok(refreshed) => refreshed,
            Err(err) => {
                app.message = format!("resume failed: {err:#}");
                return Ok(());
            }
        };
    }

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

// Relaunches a worker for a session whose previous worker has already
// exited. If the session has a captured codex_session_id, worker.rs will
// pass it to `codex resume` instead of starting a brand-new conversation.
fn relaunch_worker(session: &SessionState) -> Result<()> {
    let mut session = session.clone();
    session.status = STATUS_STARTING.to_string();
    session.worker_pid = None;
    session.child_pid = None;
    session.exit_code = None;
    session.last_error = None;
    session.updated_at = state::now_secs();
    state::write_state(&session)?;

    let mut child = Command::new(env::current_exe().context("current executable")?)
        .arg("--worker")
        .arg(&session.id)
        .current_dir(Path::new(&session.cwd))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn worker for {}", session.title))?;

    thread::spawn(move || {
        let _ = child.wait();
    });

    wait_for_socket(&session.socket);
    Ok(())
}

// Refresh titles from codex's own history (the first user message of each
// session), except where the user pinned a name via Ctrl+R. This is how an
// auto-numbered "Session N" — or any not-yet-named session — picks up a human
// title once you've talked to codex. Idempotent: only writes on an actual
// change, so steady state is just a cheap history read per refresh. Only
// sessions that already carry a codex_session_id are touched, so this can't
// clobber a rollout id the worker is about to capture.
fn sync_titles_from_history(sessions: &mut [SessionState]) {
    let firsts = state::codex_first_messages();
    if firsts.is_empty() {
        return;
    }
    for s in sessions.iter_mut() {
        if s.title_pinned {
            continue;
        }
        let Some(sid) = s.codex_session_id.as_ref() else {
            continue;
        };
        let Some(msg) = firsts.get(sid) else {
            continue;
        };
        let title = title_from_message(msg);
        if !title.is_empty() && title != s.title {
            s.title = title;
            state::write_state(s).ok();
        }
    }
}

// First non-empty line of a message, length-capped, for use as a list title.
fn title_from_message(msg: &str) -> String {
    msg.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

// Auto-numbered title for a session created without any typed text. Picks one
// past the highest existing "Session N" so repeated empty-creates don't collide.
fn default_session_title(sessions: &[SessionState]) -> String {
    let max_n = sessions
        .iter()
        .filter_map(|s| s.title.strip_prefix("Session "))
        .filter_map(|n| n.trim().parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("Session {}", max_n + 1)
}

fn create_session(title: &str, initial_prompt: Option<String>) -> Result<SessionState> {
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
        codex_session_id: None,
        codex_rollout_path: None,
        initial_prompt,
        title_pinned: false,
        last_output_at: 0,
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

// Content is left-aligned with a small margin and capped in width, so a wide
// terminal gets clean right margin rather than columns stretched across a void.
fn content_width(cols: u16) -> usize {
    (cols as usize).min(MAX_CONTENT_COLS)
}

fn render(app: &mut App) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((100, 30));
    let mut stdout = io::stdout();
    queue!(stdout, ResetColor, Hide, Clear(ClearType::All), MoveTo(0, 0))?;

    draw_header(&mut stdout, cols, app)?;
    draw_sessions(&mut stdout, app, cols, rows)?;
    draw_input(&mut stdout, app, cols, rows)?;
    draw_hint(&mut stdout, cols, rows)?;

    stdout.flush()?;
    Ok(())
}

fn draw_header(stdout: &mut io::Stdout, cols: u16, app: &App) -> Result<()> {
    let cw = content_width(cols);
    let title = "Codex Rail";
    let total = app.sessions.len();
    let count_s = if total == 1 {
        "1 session".to_string()
    } else {
        format!("{total} sessions")
    };

    // Title left, session count right, both inside the capped content width.
    let left_used = 2 + display_width(title);
    let right_w = display_width(&count_s);
    let gap = cw.saturating_sub(left_used + right_w);
    queue!(
        stdout,
        MoveTo(0, 0),
        SetForegroundColor(C_ACCENT),
        SetAttribute(Attribute::Bold),
        Print(format!("  {title}")),
        SetAttribute(Attribute::Reset),
        SetForegroundColor(C_DIM),
        Print(format!("{}{count_s}", " ".repeat(gap))),
        ResetColor
    )?;

    // Faint rule under the title.
    let rule_w = cw.saturating_sub(2);
    queue!(
        stdout,
        MoveTo(0, 1),
        SetForegroundColor(C_FAINT),
        Print(format!("  {}", "─".repeat(rule_w))),
        ResetColor
    )?;
    Ok(())
}

enum DisplayItem {
    Gap,
    Empty,
    Header(Bucket, usize),
    Row(usize),
}

fn draw_sessions(stdout: &mut io::Stdout, app: &mut App, cols: u16, rows: u16) -> Result<()> {
    app.rows.clear();
    let cw = content_width(cols);

    // Size the title/path columns to the actual content (capped), so the age
    // column sits right after the paths instead of being flung to the far
    // edge. This is the fix for the "dead gap" down the middle of every row.
    let (title_w, path_w) = session_columns(&app.sessions, cw);

    // List sits between the header rule and the bottom input box. The box top
    // is at rows-4 (see draw_input); leave one blank line above it as a gap.
    let start_y = 3_u16;
    let box_top = rows.saturating_sub(4);
    let last_list_y = box_top.saturating_sub(2);
    let max_rows = (last_list_y + 1).saturating_sub(start_y) as usize;

    if app.sessions.is_empty() {
        queue!(
            stdout,
            MoveTo(0, start_y),
            SetForegroundColor(C_DIM),
            Print(fit("  No sessions yet — press e, type your first message, then Enter.", cw)),
            ResetColor
        )?;
        return Ok(());
    }

    // Group session indices by bucket, keeping the global sort order within
    // each. Then always emit all three sections in a fixed order — even the
    // empty ones — so the panel keeps a stable shape (like Claude Code's agents
    // panel) instead of sections appearing and vanishing as statuses change.
    let mut by_bucket: [Vec<usize>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for (i, s) in app.sessions.iter().enumerate() {
        by_bucket[bucket_slot(session_bucket(s))].push(i);
    }
    let mut items: Vec<DisplayItem> = Vec::new();
    for (slot, b) in [Bucket::NeedsInput, Bucket::Working, Bucket::Stopped]
        .into_iter()
        .enumerate()
    {
        if slot > 0 {
            items.push(DisplayItem::Gap);
        }
        items.push(DisplayItem::Header(b, by_bucket[slot].len()));
        if by_bucket[slot].is_empty() {
            items.push(DisplayItem::Empty);
        } else {
            for &i in &by_bucket[slot] {
                items.push(DisplayItem::Row(i));
            }
        }
    }

    // Scroll so the selected row stays visible.
    let sel_pos = items
        .iter()
        .position(|it| matches!(it, DisplayItem::Row(i) if *i == app.selected))
        .unwrap_or(0);
    let offset = if max_rows == 0 {
        0
    } else {
        sel_pos.saturating_sub(max_rows.saturating_sub(1))
    };

    for (visible, item) in items.iter().skip(offset).take(max_rows).enumerate() {
        let y = start_y + visible as u16;
        match item {
            DisplayItem::Gap => {
                queue!(stdout, MoveTo(0, y), ResetColor, Print(fit("", cw)))?;
            }
            DisplayItem::Empty => {
                queue!(
                    stdout,
                    MoveTo(0, y),
                    ResetColor,
                    SetForegroundColor(C_FAINT),
                    Print(fit_cols("       none", cw)),
                    ResetColor
                )?;
            }
            DisplayItem::Header(b, count) => {
                draw_section_header(stdout, *b, *count, y)?;
            }
            DisplayItem::Row(index) => {
                let selected = *index == app.selected;
                draw_session_row(stdout, &app.sessions[*index], selected, y, cw, title_w, path_w)?;
                app.rows.push((y, *index));
            }
        }
    }

    Ok(())
}

// Section header: coloured glyph + name (bold, bucket colour) + dim count.
// The glyph sits at column 2 so it lines up with each row's own glyph below.
fn draw_section_header(stdout: &mut io::Stdout, b: Bucket, count: usize, y: u16) -> Result<()> {
    queue!(
        stdout,
        MoveTo(0, y),
        ResetColor,
        SetForegroundColor(bucket_color(b)),
        SetAttribute(Attribute::Bold),
        Print(format!("  {} {}", bucket_glyph(b), bucket_title(b))),
        SetAttribute(Attribute::Reset),
        SetForegroundColor(C_DIM),
        Print(format!("  {count}")),
        ResetColor
    )?;
    Ok(())
}

// Column widths for the list: sized to the widest title/path actually
// present (so short lists stay tight and the age column hugs the content),
// but capped and shrunk to fit the available width. Fixed pieces per row:
// marker(2) glyph+sp(2) gap(2) gap(2) age(4) = 12 columns.
fn session_columns(sessions: &[SessionState], cw: usize) -> (usize, usize) {
    const OVERHEAD: usize = 12;
    let max_title = sessions.iter().map(|s| display_width(&s.title)).max().unwrap_or(10);
    let max_path = sessions
        .iter()
        .map(|s| display_width(&home_tilde(&s.cwd)))
        .max()
        .unwrap_or(10);
    let mut title_w = max_title.clamp(6, 26);
    let mut path_w = max_path.clamp(6, 30);
    if title_w + path_w + OVERHEAD > cw {
        let excess = title_w + path_w + OVERHEAD - cw;
        let cut = excess.min(path_w.saturating_sub(6));
        path_w -= cut;
        let excess = (title_w + path_w + OVERHEAD).saturating_sub(cw);
        title_w = title_w.saturating_sub(excess);
    }
    (title_w, path_w)
}

// One session row, drawn in coloured segments packed into a tight left block:
//   ▌ {glyph} {title}  {path}  {age}
// The selected row gets a terracotta left bar (▌, in the left margin) and a
// warm background stretched across the whole content width. Widths are
// display-column based so CJK titles/paths stay aligned.
fn draw_session_row(
    stdout: &mut io::Stdout,
    session: &SessionState,
    selected: bool,
    y: u16,
    cw: usize,
    title_w: usize,
    path_w: usize,
) -> Result<()> {
    let bucket = session_bucket(session);
    let mut age: String = format_age(last_activity_secs(session)).chars().take(4).collect();
    while age.chars().count() < 4 {
        age.insert(0, ' '); // right-align in a fixed 4-wide column
    }

    let title_s = fit_cols(&session.title, title_w);
    let path_s = fit_cols_tail(&home_tilde(&session.cwd), path_w);
    let title_color = if selected { C_SELTITLE } else { C_TITLE };

    if selected {
        queue!(stdout, SetBackgroundColor(C_SEL_BG))?;
    } else {
        queue!(stdout, ResetColor)?;
    }
    queue!(stdout, MoveTo(0, y))?;
    if selected {
        queue!(stdout, SetForegroundColor(C_ACCENT), Print("▌"), Print(" "))?;
    } else {
        queue!(stdout, Print("  "))?;
    }
    queue!(
        stdout,
        SetForegroundColor(bucket_color(bucket)),
        Print(format!("{} ", bucket_glyph(bucket))),
        SetForegroundColor(title_color),
        Print(title_s),
        Print("  "),
        SetForegroundColor(C_DIM),
        Print(path_s),
        Print("  "),
        Print(age)
    )?;

    // Extend the selection highlight across the rest of the content width.
    if selected {
        let used = 12 + title_w + path_w;
        if cw > used {
            queue!(stdout, Print(" ".repeat(cw - used)))?;
        }
    }
    queue!(stdout, ResetColor)?;
    Ok(())
}

fn draw_input(stdout: &mut io::Stdout, app: &App, cols: u16, rows: u16) -> Result<()> {
    let cw = content_width(cols);
    let box_top = rows.saturating_sub(4);
    let box_w = cw.saturating_sub(2); // box occupies columns [2, 2+box_w)
    if box_w < 6 {
        return Ok(());
    }
    let inner = box_w - 2; // usable columns between the two side borders

    // Top border. A transient status message rides on it as a box title
    // (amber), e.g. "╭─ renamed ─────╮"; otherwise it's a plain faint border.
    let (top, top_color) = if app.message.is_empty() {
        (format!("╭{}╮", "─".repeat(box_w - 2)), C_ACCENT_DIM)
    } else {
        let msg = format!(" {} ", app.message);
        let msg_w = display_width(&msg).min(box_w.saturating_sub(4));
        let msg_s = fit_cols(&msg, msg_w);
        let rest = (box_w - 2).saturating_sub(1 + msg_w);
        (format!("╭─{}{}╮", msg_s, "─".repeat(rest)), C_NEEDS)
    };
    queue!(
        stdout,
        MoveTo(0, box_top),
        ResetColor,
        SetForegroundColor(top_color),
        Print(format!("  {top}")),
        ResetColor
    )?;

    // Middle line: "│ ❯ <text> │". In Normal mode this is a dim hint; while
    // typing it's the entered name. The mode (new/rename) shows on the border.
    let (body_text, body_color) = match app.mode {
        Mode::Normal => (
            "press e, or just start typing, to send codex a first message".to_string(),
            C_DIM,
        ),
        Mode::New | Mode::Rename => (app.input.clone(), C_TITLE),
    };
    queue!(
        stdout,
        MoveTo(0, box_top + 1),
        ResetColor,
        SetForegroundColor(C_ACCENT_DIM),
        Print("  │"),
        SetForegroundColor(C_ACCENT),
        Print(" ❯ "),
        SetForegroundColor(body_color),
        Print(fit_cols(&body_text, inner.saturating_sub(3))),
        SetForegroundColor(C_ACCENT_DIM),
        Print("│"),
        ResetColor
    )?;

    // Bottom border.
    queue!(
        stdout,
        MoveTo(0, box_top + 2),
        SetForegroundColor(C_ACCENT_DIM),
        Print(format!("  ╰{}╯", "─".repeat(box_w - 2))),
        ResetColor
    )?;
    Ok(())
}

// Compact key hints on the last line, using terse symbols instead of the old
// full-width "Ctrl-X Ctrl-X" wall of text.
fn draw_hint(stdout: &mut io::Stdout, cols: u16, rows: u16) -> Result<()> {
    // Uses the full terminal width and display-width-aware fitting so the "·"
    // separators (2 bytes each) don't get miscounted and clip the last word.
    let hint = "w/s move · enter attach · e new · ^R rename · ^X ^X stop · esc esc quit";
    queue!(
        stdout,
        MoveTo(0, rows.saturating_sub(1)),
        ResetColor,
        SetForegroundColor(C_DIM),
        Print(fit_cols(&format!("  {hint}"), cols as usize)),
        ResetColor
    )?;
    Ok(())
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
