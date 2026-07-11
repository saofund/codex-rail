use crate::attach;
use crate::autopilot;
use crate::distill;
use crate::progress;
use crate::update;
use crate::state::{
    self, SessionState, STATUS_EXITED, STATUS_FAILED, STATUS_RUNNING, STATUS_STARTING,
    STATUS_STOPPING,
};
use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STOP_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const EXIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const REFRESH_INTERVAL: Duration = Duration::from_millis(700);
// How often to re-scan ~/.codex/sessions for newly-created sessions in this cwd.
// The scan reads a header line per rollout, so it's throttled well above the
// 700ms UI refresh; new codex sessions still appear within this window.
// Overridable via CODEX_RAIL_ADOPT_MS (tests set it low).
fn adopt_interval() -> Duration {
    Duration::from_millis(
        env::var("CODEX_RAIL_ADOPT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20_000),
    )
}
// Preview tail: enough to contain the last agent_message even when codex writes
// large events (a single message line can be tens of KB).
const ROLLOUT_TAIL_BYTES: u64 = 256 * 1024;
// When recovering a rollout for a session whose worker never captured one, only
// accept a codex rollout that started within this window of the session's
// creation. Real prompted sessions start their rollout within seconds; a wide
// gap means we're not sure it's the same session, so we leave it unresolved
// (row falls back to its path) rather than mislabel it.
const ROLLOUT_MATCH_WINDOW_SECS: u64 = 3600;

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
const C_DONE: Color = Color::Rgb { r: 100, g: 200, b: 210 }; // teal: finished task
const C_STOPPED: Color = Color::Rgb { r: 140, g: 148, b: 168 }; // grey (readable, not "black")

// Cap the content region so age/metadata don't fly to the far edge of an
// ultra-wide terminal. The message-preview column fills the middle, so a wider
// cap just shows more of each codex message (no dead gap) — 120 uses a normal
// wide terminal fully while still reining in a 200-column one.
const MAX_CONTENT_COLS: usize = 120;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activity {
    Active,
    Waiting,
}

// Incremental scan state for one session's rollout. We remember how far into
// the file we've read and the last turn-lifecycle marker we saw, so each
// refresh only reads the bytes appended since last time — O(new bytes), not
// O(file). This is what makes Working detection reliable: a real turn can run
// for over a minute writing NOTHING to the rollout (codex is generating), and
// spans far more than any fixed tail window, so neither an mtime heuristic nor
// a bounded tail-scan can see it. Tracking the last marker across refreshes
// keeps "Active" latched from task_started until task_complete, silence or not.
#[derive(Clone)]
struct Lifecycle {
    path: String,
    offset: u64,
    last: Option<Activity>,
}

// Read the bytes appended to the rollout since we last looked and update the
// latched lifecycle state. codex writes an `event_msg` with
// `payload.type = "task_started"` when a turn begins and `"task_complete"` when
// it finishes; the newest of the two is the current state. Undocumented,
// reverse-engineered codex-cli format (verified against cli_version 0.142.5).
fn scan_lifecycle(lc: &mut Lifecycle, path: &str) -> Activity {
    if lc.path != path {
        lc.path = path.to_string();
        lc.offset = 0;
        lc.last = None;
    }
    let Ok(mut file) = fs::File::open(path) else {
        return lc.last.unwrap_or(Activity::Waiting);
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < lc.offset {
        // File shrank (rotated/truncated) — rescan from the top.
        lc.offset = 0;
        lc.last = None;
    }
    if len > lc.offset && file.seek(SeekFrom::Start(lc.offset)).is_ok() {
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_ok() {
            // Only consume up to the last complete line; a trailing partial
            // line (codex mid-write of a big event) is left for next time.
            if let Some(nl) = buf.iter().rposition(|&b| b == b'\n') {
                for line in buf[..=nl].split(|&b| b == b'\n') {
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
                        continue;
                    };
                    match value
                        .get("payload")
                        .and_then(|p| p.get("type"))
                        .and_then(|t| t.as_str())
                    {
                        Some("task_started") => lc.last = Some(Activity::Active),
                        Some("task_complete") => lc.last = Some(Activity::Waiting),
                        _ => {}
                    }
                }
                lc.offset += (nl + 1) as u64;
            }
        }
    }
    // No marker ever seen (turn hasn't started) → waiting for you.
    lc.last.unwrap_or(Activity::Waiting)
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

// Newest agent (codex) message in the rollout — the row's activity preview,
// mirroring Claude Code's agents panel which shows each agent's latest line.
// Best-effort over the same reverse-engineered rollout format as the status
// classifier; any parse issue just yields None and the row falls back to path.
fn last_agent_message(path: &str) -> Option<String> {
    let tail = read_tail(Path::new(path))?;
    for line in tail.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(payload) = value.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|t| t.as_str()) != Some("agent_message") {
            continue;
        }
        if let Some(msg) = payload.get("message").and_then(|m| m.as_str()) {
            if state::is_synthetic_marker(msg) {
                continue; // e.g. codex's "<EXTERNAL SESSION IMPORTED>" — keep looking
            }
            let preview = preview_line(msg);
            if !preview.is_empty() {
                return Some(preview);
            }
        }
    }
    None
}

// Collapse a message to a single tidy preview line: first non-empty line with
// runs of whitespace squeezed to single spaces.
fn preview_line(s: &str) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

// Sessions are grouped into these buckets in the list, mirroring Claude
// Code's own agents panel (Needs input / Working / Stopped).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bucket {
    NeedsInput,
    Working,
    Done,
    Stopped,
}

// Bucket for a session, using the activity computed once per reload (see
// App::refresh_activity). Reading it from a map keeps draw/sort pure — no
// rollout I/O happens while rendering, only when state is refreshed.
fn bucket_of(activity: &HashMap<String, Activity>, s: &SessionState) -> Bucket {
    let base = match s.status.as_str() {
        STATUS_RUNNING => match activity.get(&s.id).copied().unwrap_or(Activity::Waiting) {
            Activity::Waiting => Bucket::NeedsInput,
            Activity::Active => Bucket::Working,
        },
        STATUS_STARTING | STATUS_STOPPING => Bucket::Working,
        _ => Bucket::Stopped, // exited, failed, unknown
    };
    // A distillation is a one-shot TASK, not a chat: once its style file has
    // landed and it's no longer actively working, it reads as "Done", not "Needs
    // input" (it isn't waiting on you) nor "Stopped" (it succeeded).
    if s.distill_version.is_some() && !matches!(base, Bucket::Working) && distill_done(s) {
        return Bucket::Done;
    }
    base
}

// Has this distill session's output file been written yet?
fn distill_done(s: &SessionState) -> bool {
    match s.distill_version {
        Some(v) => state::distill_dir()
            .join(format!("style-v{v:03}.md"))
            .exists(),
        None => false,
    }
}

fn bucket_rank(b: Bucket) -> u8 {
    match b {
        Bucket::NeedsInput => 0, // things needing your attention float to the top
        Bucket::Working => 1,
        Bucket::Done => 2,
        Bucket::Stopped => 3,
    }
}

// Fixed slot index for each bucket, used to always lay the sections out in the
// same order (Needs input / Working / Done / Stopped) whether or not they're empty.
fn bucket_slot(b: Bucket) -> usize {
    match b {
        Bucket::NeedsInput => 0,
        Bucket::Working => 1,
        Bucket::Done => 2,
        Bucket::Stopped => 3,
    }
}

fn bucket_title(b: Bucket) -> &'static str {
    match b {
        Bucket::NeedsInput => "Needs input",
        Bucket::Working => "Working",
        Bucket::Done => "Done",
        Bucket::Stopped => "Stopped",
    }
}

fn bucket_color(b: Bucket) -> Color {
    match b {
        Bucket::NeedsInput => C_NEEDS,
        Bucket::Working => C_WORKING,
        Bucket::Done => C_DONE,
        Bucket::Stopped => C_STOPPED,
    }
}

fn bucket_glyph(b: Bucket) -> char {
    match b {
        Bucket::NeedsInput => '●', // filled amber: wants your attention
        Bucket::Working => '✻',    // busy
        Bucket::Done => '✓',       // finished task, product delivered
        Bucket::Stopped => '○',    // hollow: inactive
    }
}

// Age shown in the right-hand column. Deliberately keyed off updated_at only —
// the time of the last real state change (start/resume/stop) — NOT
// last_output_at. A working codex streams to its PTY constantly, and the worker
// records that every ~2s; folding it in made the age flicker 0→1→2→0 forever on
// any active session. updated_at ticks up as a steady clock instead.
fn last_activity_secs(s: &SessionState) -> u64 {
    state::now_secs().saturating_sub(s.updated_at)
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
    spawn_terminal_watchdog();
    let mut app = App::load()?;
    let result = manager_loop(&mut terminal, &mut app);
    terminal.leave().ok();
    result
}

// True once our controlling terminal has hung up (the pty master closed). A
// live tty answers TIOCGWINSZ with the window size; a hung-up one fails with
// EIO (and ENXIO/EBADF as it's torn down). This is a read-only ioctl, so —
// unlike a read() — it never steals a keystroke from crossterm's input loop.
fn terminal_is_dead() -> bool {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 {
        return false;
    }
    matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EIO) | Some(libc::ENXIO) | Some(libc::EBADF)
    )
}

// A tiny watchdog thread. If the terminal dies without a clean quit (SSH drop,
// window closed, parent shell exits), crossterm's event::read() busy-loops on
// the dead tty forever and the main loop never regains control — so no check
// *inside* that loop can save us. This independent thread notices the hangup
// and force-exits the process, so a manager can never end up as a stray
// 100%-CPU orphan (this host's PID 1 never reaps, so it would linger for good).
// The workers it spawned are separate processes and keep running, as intended.
fn spawn_terminal_watchdog() {
    thread::spawn(|| loop {
        thread::sleep(Duration::from_millis(300));
        if terminal_is_dead() {
            std::process::exit(0);
        }
    });
}

fn manager_loop(terminal: &mut TerminalSession, app: &mut App) -> Result<()> {
    let mut last_refresh = Instant::now();
    render(app)?;

    loop {
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.reload()?;
            // Advance any autopilots (create/nudge pilots, deliver replies). If it
            // acted, reload so a freshly-spawned pilot appears immediately.
            if app.drive_autopilot() {
                app.reload()?;
            }
            last_refresh = Instant::now();
            render(app)?;
        }

        // A background distillation prep may have finished (launch the session) or
        // still be running (tick its elapsed-time status). Repaint if it changed.
        if app.distill.is_some() && poll_distillation(app)? {
            render(app)?;
        }

        // Drive the background codex-session scan: show its real progress while it
        // runs (only when it's actually running AND nothing else is on the status
        // line — never clobber a confirm/warning), merge the imported rows in when
        // it finishes. The manager already rendered, so this never blocks startup.
        if app.adopt_job.is_some() {
            if app.poll_adopt() {
                app.merge_adopted();
                app.refresh_derived();
                app.sort_for_display();
                if app.message.starts_with("importing codex history") {
                    app.message.clear();
                }
                render(app)?;
            } else if app.message.is_empty() || app.message.starts_with("importing codex history") {
                if let Some(status) = app.adopt_status() {
                    app.message = status;
                    render(app)?;
                }
            }
        }

        // The background update check returns once; surface it as a header note.
        if let Some(rx) = &app.update_rx {
            if let Ok(result) = rx.try_recv() {
                app.update_available = result;
                app.update_rx = None;
                render(app)?;
            }
        }
        // A finished `/update` reports its result on the status line.
        if let Some(rx) = &app.update_apply {
            if let Ok(msg) = rx.try_recv() {
                app.message = msg;
                app.update_apply = None;
                render(app)?;
            }
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
                if handle_mouse(mouse.kind, mouse.row, mouse.column, app, terminal)? {
                    return Ok(());
                }
                render(app)?;
            }
            Event::Resize(_, _) => {
                app.invalidate_frame();
                render(app)?;
            }
            // A bracketed paste arrives as one event (not char-by-char), so it can't
            // be dropped or misread as keystrokes. Newlines collapse to spaces — the
            // composer is a single line; codex still receives it as one message.
            Event::Paste(text) => {
                if handle_paste(&text, app) {
                    render(app)?;
                }
            }
            _ => {}
        }
    }
}

// Insert pasted text into the composer. In Normal mode a paste opens the new-
// session composer (like typing does); while composing/renaming it appends.
fn handle_paste(text: &str, app: &mut App) -> bool {
    let cleaned: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if cleaned.is_empty() {
        return false;
    }
    match app.mode {
        Mode::Normal => {
            app.mode = Mode::New;
            app.input.clear();
            app.input.push_str(&cleaned);
            app.stop_confirm = None;
            app.exit_confirm = None;
        }
        Mode::New | Mode::Rename => {
            app.input.push_str(&cleaned);
        }
    }
    true
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
    // Set when the user tried to attach a maybe-live imported session; a second
    // Enter within the window confirms the resume (which starts a 2nd codex).
    resume_confirm: Option<(String, Instant)>,
    // Highlighted row in the composer's slash-command palette (when the input
    // starts with '/').
    slash_sel: usize,
    rows: Vec<(u16, usize)>,
    // Memoized rollout recovery per session id: Some(path) once resolved, None
    // once we've scanned and found no confident match. Keeps the (potentially
    // large) codex sessions directory from being re-scanned every refresh.
    rollout_cache: HashMap<String, Option<String>>,
    // Per-session derived state, computed once per reload (not per draw) so
    // rendering does no file I/O: turn activity (via incremental lifecycle
    // scan), its scan cursor, and the latest-message preview line.
    activity: HashMap<String, Activity>,
    lifecycle: HashMap<String, Lifecycle>,
    preview: HashMap<String, String>,
    // Previous rendered frame, one string per screen row, for diff-based
    // drawing (only changed rows are rewritten — no full clear, no flicker).
    prev_frame: Vec<String>,
    // In-flight archive-distillation prep. Scanning the codex + Claude archives
    // takes several seconds, so it runs on a worker thread; the manager loop
    // polls this each tick, shows an elapsed-time status, and launches the codex
    // session once the corpus is ready — the UI never freezes.
    distill: Option<DistillJob>,
    // Codex sessions for the manager's cwd that rail didn't start (see
    // adopt_codex_sessions), keyed by codex session id and accumulated across
    // background scans; merged into the list every reload. The scan runs OFF the
    // UI thread so the manager appears instantly and never looks frozen.
    adopted: HashMap<String, SessionState>,
    adopt_job: Option<AdoptJob>,
    adopt_started: Option<Instant>, // throttles rescans
    adopt_since: SystemTime,        // rescans read only rollouts modified after this
    // Background "is a newer build on GitHub?" check; its result (a newer short
    // commit, or None) surfaces as a quiet header note. Best-effort — offline or
    // GitHub-unreachable just leaves it None.
    update_rx: Option<mpsc::Receiver<Option<String>>>,
    update_available: Option<String>,
    // In-flight `/update` apply; its result message lands on the status line.
    update_apply: Option<mpsc::Receiver<String>>,
    // Column span of the header "↑ update available" note (row 0), so a click on
    // it triggers the update. Set each render; None when no update is pending.
    update_click: Option<(u16, u16)>,
    // Autopilot control per main session id (Space toggles it). Loaded from each
    // session's autopilot.json every reload; the driver advances the reply cycle
    // each tick. `pilot_to_main` is the reverse link (a pilot session's id -> the
    // main it answers) so the list can render pilots grouped under their main.
    autopilot: HashMap<String, autopilot::AutopilotState>,
    pilot_to_main: HashMap<String, String>,
}

// A background codex-session scan in flight. The startup scan reads every
// rollout; a rescan reads only files modified since the last scan (cheap).
// Progress is shared so the UI can draw a real bar of what's being scanned.
struct AdoptJob {
    rx: mpsc::Receiver<Vec<SessionState>>,
    progress: Arc<AdoptProgress>,
    scan_start: SystemTime,
}
struct AdoptProgress {
    done: AtomicUsize,
    total: AtomicUsize,
    current: Mutex<String>, // basename of the file being scanned
}

// A background `distill::prepare()` in flight: the channel it will deliver on,
// and when it started (for the elapsed-time status).
struct DistillJob {
    rx: mpsc::Receiver<Result<distill::DistillPrep>>,
    started: Instant,
}

impl App {
    fn load() -> Result<Self> {
        let mut app = Self {
            sessions: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            message: String::new(),
            stop_confirm: None,
            exit_confirm: None,
            resume_confirm: None,
            slash_sel: 0,
            rows: Vec::new(),
            rollout_cache: HashMap::new(),
            activity: HashMap::new(),
            lifecycle: HashMap::new(),
            preview: HashMap::new(),
            prev_frame: Vec::new(),
            distill: None,
            adopted: HashMap::new(),
            adopt_job: None,
            adopt_started: None,
            adopt_since: UNIX_EPOCH,
            update_rx: None,
            update_available: None,
            update_apply: None,
            update_click: None,
            autopilot: HashMap::new(),
            pilot_to_main: HashMap::new(),
        };
        app.sessions = state::load_sessions()?;
        resolve_missing_rollouts(&mut app.sessions, &mut app.rollout_cache);
        app.spawn_adopt_scan(); // off-thread; adopted rows merge in when it finishes
        // Fire the update check off the UI thread; the header shows a note if a
        // newer build lands. Skipped when CODEX_RAIL_NO_UPDATE_CHECK is set;
        // CODEX_RAIL_FAKE_UPDATE forces the note (tests, without hitting GitHub).
        let forced = env::var("CODEX_RAIL_FAKE_UPDATE")
            .ok()
            .filter(|s| !s.is_empty());
        if forced.is_some() || env::var_os("CODEX_RAIL_NO_UPDATE_CHECK").is_none() {
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(forced.or_else(update::newer_available));
            });
            app.update_rx = Some(rx);
        }
        app.merge_adopted();
        app.load_autopilot();
        app.refresh_derived();
        app.sort_for_display();
        Ok(app)
    }

    // Load each session's autopilot.json into `autopilot` (keyed by main id) and
    // build the reverse `pilot_to_main` link so pilots render grouped under their
    // main. Cheap: the file exists only for autopiloted sessions.
    fn load_autopilot(&mut self) {
        self.autopilot.clear();
        self.pilot_to_main.clear();
        for s in &self.sessions {
            if let Some(st) = autopilot::load(&s.id) {
                if let Some(pid) = &st.pilot_id {
                    self.pilot_to_main.insert(pid.clone(), s.id.clone());
                }
                self.autopilot.insert(s.id.clone(), st);
            }
        }
    }

    // Space on the selected session turns autopilot on/off. On, rail will answer
    // that session for you (via a pilot session) whenever it finishes a turn.
    fn toggle_autopilot(&mut self) {
        let Some((id, adopted, title)) = self
            .current()
            .map(|s| (s.id.clone(), s.adopted, s.title.clone()))
        else {
            return;
        };
        if self.pilot_to_main.contains_key(&id) {
            self.message = "that's a pilot — toggle autopilot on its main session".to_string();
            return;
        }
        if adopted {
            self.message = "attach this imported session once before autopiloting it".to_string();
            return;
        }
        let on = self
            .autopilot
            .get(&id)
            .map(|s| s.enabled)
            .unwrap_or(false);
        if on {
            autopilot::remove(&id);
            self.autopilot.remove(&id);
            self.message = format!("autopilot off for \u{201c}{}\u{201d}", clip_title(&title));
        } else {
            // (re-)enable: fresh cycle, keep any existing pilot to reuse.
            let mut st = autopilot::load(&id).unwrap_or_default();
            st.enabled = true;
            st.replies = 0;
            st.phase = autopilot::Phase::Idle;
            st.main_marker.clear();
            st.pending_reply.clear();
            st.last_reason = None;
            autopilot::save(&id, &st);
            self.message = format!(
                "autopilot ON \u{2014} I'll answer \u{201c}{}\u{201d} for you (up to {} replies) \u{00b7} Space to stop",
                clip_title(&title),
                st.cap
            );
            self.autopilot.insert(id, st);
        }
    }

    // Is this session alive and idle-waiting for you (Needs input)? The trigger
    // condition for an autopilot reply.
    fn session_needs_input(&self, id: &str) -> bool {
        self.sessions
            .iter()
            .find(|s| s.id == id)
            .map(|s| bucket_of(&self.activity, s) == Bucket::NeedsInput)
            .unwrap_or(false)
    }

    fn session_alive(&self, id: &str) -> bool {
        self.sessions
            .iter()
            .any(|s| s.id == id && s.status == STATUS_RUNNING)
    }

    fn session_socket(&self, id: &str) -> Option<String> {
        self.sessions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.socket.clone())
    }

    fn session_rollout(&self, id: &str) -> Option<String> {
        self.sessions
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.codex_rollout_path.clone())
    }

    // Advance the autopilot reply cycle for every enabled main session, once per
    // refresh. Returns true if it changed anything (spawned a pilot, injected a
    // reply, paused) so the caller can reload + repaint. Cheap when idle.
    fn drive_autopilot(&mut self) -> bool {
        if self.autopilot.is_empty() {
            return false;
        }
        let mains: Vec<String> = self
            .autopilot
            .iter()
            .filter(|(_, st)| st.enabled)
            .map(|(id, _)| id.clone())
            .collect();
        let mut changed = false;
        for id in mains {
            changed |= self.drive_one_autopilot(&id);
        }
        changed
    }

    fn drive_one_autopilot(&mut self, main_id: &str) -> bool {
        let mut st = match self.autopilot.get(main_id) {
            Some(s) => s.clone(),
            None => return false,
        };
        if !self.sessions.iter().any(|s| s.id == main_id) {
            autopilot::remove(main_id); // main gone
            self.autopilot.remove(main_id);
            return true;
        }
        if st.replies >= st.cap {
            let cap = st.cap;
            return self.pause_autopilot(
                main_id,
                &mut st,
                format!("reached the {cap}-reply limit \u{2014} your turn"),
            );
        }
        let main_msg = self.preview.get(main_id).cloned().unwrap_or_default();
        let mut changed = false;
        match st.phase {
            autopilot::Phase::Idle => {
                // fire on a NEW completed main turn (and only when idle-waiting)
                if self.session_needs_input(main_id)
                    && !main_msg.is_empty()
                    && main_msg != st.main_marker
                {
                    st.main_marker = main_msg.clone();
                    match st.pilot_id.clone() {
                        None => match self.spawn_pilot(main_id, &main_msg) {
                            Some(pid) => {
                                st.pilot_id = Some(pid);
                                st.pilot_marker.clear();
                                st.phase = autopilot::Phase::Generating;
                                self.message = "autopilot: pilot thinking\u{2026}".to_string();
                                changed = true;
                            }
                            None => {
                                return self.pause_autopilot(
                                    main_id,
                                    &mut st,
                                    "couldn't start the pilot session".to_string(),
                                );
                            }
                        },
                        Some(pid) => {
                            // baseline the pilot's current reply, then nudge it
                            st.pilot_marker = self
                                .session_rollout(&pid)
                                .and_then(|r| autopilot::last_agent_message_full(&r))
                                .unwrap_or_default();
                            match self.session_socket(&pid) {
                                Some(sock) => {
                                    let prompt = autopilot::continue_prompt(&main_msg);
                                    match autopilot::inject(&sock, format!("{prompt}\r").as_bytes())
                                    {
                                        Ok(true) => {
                                            st.phase = autopilot::Phase::Generating;
                                            self.message =
                                                "autopilot: pilot thinking\u{2026}".to_string();
                                            changed = true;
                                        }
                                        // pilot busy/attached/transient -> retry next turn
                                        _ => st.main_marker.clear(),
                                    }
                                }
                                None => st.main_marker.clear(),
                            }
                        }
                    }
                }
            }
            autopilot::Phase::Generating => {
                match st.pilot_id.clone() {
                    Some(pid) if self.session_needs_input(&pid) => {
                        let reply = self
                            .session_rollout(&pid)
                            .and_then(|r| autopilot::last_agent_message_full(&r))
                            .unwrap_or_default();
                        if !reply.is_empty() && reply != st.pilot_marker {
                            match autopilot::parse_reply(&reply) {
                                autopilot::Reply::Done => {
                                    return self.pause_autopilot(
                                        main_id,
                                        &mut st,
                                        "pilot judged the task complete".to_string(),
                                    );
                                }
                                autopilot::Reply::HandBack(reason) => {
                                    return self.pause_autopilot(main_id, &mut st, reason);
                                }
                                autopilot::Reply::Send(text) => {
                                    st.pending_reply = text;
                                    st.phase = autopilot::Phase::Delivering;
                                    changed = true;
                                }
                            }
                        }
                    }
                    Some(pid) if !self.session_alive(&pid) => {
                        return self.pause_autopilot(
                            main_id,
                            &mut st,
                            "pilot session exited".to_string(),
                        );
                    }
                    Some(_) => {} // pilot still working
                    None => st.phase = autopilot::Phase::Idle,
                }
            }
            autopilot::Phase::Delivering => {
                if let Some(sock) = self.session_socket(main_id) {
                    // retries next tick if a human is attached (worker refuses us)
                    if let Ok(true) =
                        autopilot::inject(&sock, format!("{}\r", st.pending_reply).as_bytes())
                    {
                        st.replies += 1;
                        st.pending_reply.clear();
                        st.phase = autopilot::Phase::Idle;
                        self.message = format!("autopilot: replied ({}/{})", st.replies, st.cap);
                        changed = true;
                    }
                }
            }
        }
        autopilot::save(main_id, &st);
        self.autopilot.insert(main_id.to_string(), st);
        changed
    }

    // Disable autopilot for a main with a reason (cap hit, handback, pilot died);
    // persists enabled=false + reason so the row shows why, and surfaces it.
    fn pause_autopilot(
        &mut self,
        main_id: &str,
        st: &mut autopilot::AutopilotState,
        reason: String,
    ) -> bool {
        st.enabled = false;
        st.phase = autopilot::Phase::Idle;
        st.pending_reply.clear();
        st.last_reason = Some(reason.clone());
        autopilot::save(main_id, st);
        self.autopilot.insert(main_id.to_string(), st.clone());
        self.message = format!("autopilot paused: {reason}");
        true
    }

    // Create the pilot session for a main: a real, visible codex session (grouped
    // under its main in the list) whose task is to write the user's replies. Runs
    // read-only + autonomous, pinned to the main's cwd, inheriting the user's model.
    fn spawn_pilot(&mut self, main_id: &str, main_msg: &str) -> Option<String> {
        let (main_cwd, main_title, rollout) = {
            let m = self.sessions.iter().find(|s| s.id == main_id)?;
            (
                PathBuf::from(&m.cwd),
                clip_title(&m.title),
                m.codex_rollout_path.clone().unwrap_or_default(),
            )
        };
        let style_path = latest_style_file()
            .map(|f| state::distill_dir().join(f).to_string_lossy().to_string())
            .unwrap_or_else(|| "(no distilled style file yet — use good judgment)".to_string());
        let prompt = autopilot::initial_prompt(&style_path, main_id, &rollout, main_msg);
        let _ = distill::ensure_trusted(&main_cwd);
        // The pilot writes a reply — it doesn't need the user's deep-reasoning
        // default, so run it lighter (faster per turn). Overridable.
        let effort =
            env::var("CODEX_RAIL_PILOT_EFFORT").unwrap_or_else(|_| "medium".to_string());
        let mut codex_args = vec![
            "-C".to_string(),
            main_cwd.to_string_lossy().to_string(),
            "-s".to_string(),
            "read-only".to_string(),
            "-a".to_string(),
            "never".to_string(),
        ];
        if !effort.is_empty() {
            codex_args.push("-c".to_string());
            codex_args.push(format!("model_reasoning_effort={effort}"));
        }
        // Optional pilot model override (else inherits the user's config default).
        if let Ok(model) = env::var("CODEX_RAIL_PILOT_MODEL") {
            if !model.is_empty() {
                codex_args.push("-m".to_string());
                codex_args.push(model);
            }
        }
        let title = format!("\u{21b3} pilot \u{00b7} {main_title}");
        match create_session_in(&title, Some(prompt), Some(main_cwd), codex_args, None) {
            Ok(sess) => Some(sess.id),
            Err(err) => {
                self.message = format!("pilot launch failed: {err:#}");
                None
            }
        }
    }

    fn reload(&mut self) -> Result<()> {
        let selected_id = self.current().map(|s| s.id.clone());
        self.sessions = state::load_sessions()?;
        resolve_missing_rollouts(&mut self.sessions, &mut self.rollout_cache);
        sync_titles_from_history(&mut self.sessions);
        // Start a background rescan when idle and the throttle has elapsed (it
        // reads only rollouts modified since the last scan), then merge whatever
        // has already been imported.
        if self.adopt_job.is_none()
            && self
                .adopt_started
                .map(|t| t.elapsed() >= adopt_interval())
                .unwrap_or(false)
        {
            self.spawn_adopt_scan();
        }
        self.merge_adopted();
        self.load_autopilot();
        self.refresh_derived();
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

    // Recompute the per-session derived state that draws/sorts read from —
    // activity (bucket) and the message preview — ONCE per reload, so rendering
    // stays pure formatting with no file I/O. Activity uses the incremental
    // lifecycle scanner (see scan_lifecycle); the preview reads the rollout tail.
    fn refresh_derived(&mut self) {
        let items: Vec<(String, Option<String>, bool)> = self
            .sessions
            .iter()
            .map(|s| (s.id.clone(), s.codex_rollout_path.clone(), s.status == STATUS_RUNNING))
            .collect();
        for (id, path, running) in &items {
            match path {
                Some(p) if *running => {
                    let lc = self.lifecycle.entry(id.clone()).or_insert_with(|| Lifecycle {
                        path: String::new(),
                        offset: 0,
                        last: None,
                    });
                    self.activity.insert(id.clone(), scan_lifecycle(lc, p));
                }
                _ => {
                    self.activity.insert(id.clone(), Activity::Waiting);
                }
            }
            match path.as_deref().and_then(last_agent_message) {
                Some(msg) => {
                    self.preview.insert(id.clone(), msg);
                }
                None => {
                    self.preview.remove(id);
                }
            }
        }
        // Drop cache entries for sessions that no longer exist.
        let ids: std::collections::HashSet<&String> = items.iter().map(|(id, _, _)| id).collect();
        self.activity.retain(|k, _| ids.contains(k));
        self.lifecycle.retain(|k, _| ids.contains(k));
        self.preview.retain(|k, _| ids.contains(k));
        self.rollout_cache.retain(|k, _| ids.contains(k));
    }

    // Start a background scan of ~/.codex/sessions for this cwd. First call is a
    // full scan (adopt_since = epoch); later calls read only rollouts modified
    // since the previous scan (cheap). Excludes sessions already in the list or
    // dismissed — excludes by BOTH codex id and rollout path, since an old rail
    // session may never have captured its codex id but resolve_missing_rollouts
    // recovers its path, which matches the candidate's (so it isn't duplicated).
    fn spawn_adopt_scan(&mut self) {
        if self.adopt_job.is_some() {
            return;
        }
        let cwd = env::current_dir().unwrap_or_default();
        let mut exclude: HashSet<String> = state::adopt_dismissed();
        for s in &self.sessions {
            if let Some(id) = &s.codex_session_id {
                exclude.insert(id.clone());
            }
            if let Some(p) = &s.codex_rollout_path {
                exclude.insert(p.clone());
            }
        }
        let since = self.adopt_since;
        let scan_start = SystemTime::now();
        let progress = Arc::new(AdoptProgress {
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            current: Mutex::new(String::new()),
        });
        let prog = progress.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(adopt_codex_sessions(&cwd, &exclude, since, &prog));
        });
        self.adopt_job = Some(AdoptJob {
            rx,
            progress,
            scan_start,
        });
        self.adopt_started = Some(Instant::now());
    }

    // Poll the background scan. On completion, accumulate the found sessions into
    // self.adopted (keyed by codex id) and advance the mtime cutoff. Returns true
    // when it finishes so the caller repaints (the imported rows just appeared).
    fn poll_adopt(&mut self) -> bool {
        let Some(job) = &self.adopt_job else {
            return false;
        };
        match job.rx.try_recv() {
            Ok(found) => {
                self.adopt_since = job.scan_start;
                for s in found {
                    if let Some(id) = s.codex_session_id.clone() {
                        self.adopted.insert(id, s);
                    }
                }
                self.adopt_job = None;
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.adopt_job = None;
                true
            }
        }
    }

    // A live one-line progress string while a scan runs, else None: a real bar of
    // files scanned plus the file currently being read.
    fn adopt_status(&self) -> Option<String> {
        let job = self.adopt_job.as_ref()?;
        let total = job.progress.total.load(Ordering::Relaxed);
        let done = job.progress.done.load(Ordering::Relaxed);
        if total == 0 {
            return Some("importing codex history \u{2026}".to_string());
        }
        let bar = progress::render(
            16,
            done as f64 / total as f64,
            progress::Style {
                fill: C_ACCENT,
                track: C_FAINT,
            },
        );
        let cur = job
            .progress
            .current
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        Some(format!(
            "importing codex history {bar} {done}/{total} \u{00b7} {cur}"
        ))
    }

    // Merge the accumulated imported sessions into the list, skipping any now
    // backed by a real rail session (by id / codex id / rollout path) or dismissed.
    fn merge_adopted(&mut self) {
        if self.adopted.is_empty() {
            return;
        }
        let dismissed = state::adopt_dismissed();
        let mut have: HashSet<String> = HashSet::new();
        for s in &self.sessions {
            have.insert(s.id.clone());
            if let Some(c) = &s.codex_session_id {
                have.insert(c.clone());
            }
            if let Some(p) = &s.codex_rollout_path {
                have.insert(p.clone());
            }
        }
        let mut adds: Vec<SessionState> = Vec::new();
        for (sid, a) in &self.adopted {
            let dup = dismissed.contains(sid)
                || have.contains(&a.id)
                || a.codex_session_id.as_ref().is_some_and(|c| have.contains(c))
                || a.codex_rollout_path.as_ref().is_some_and(|p| have.contains(p));
            if !dup {
                adds.push(a.clone());
            }
        }
        self.sessions.extend(adds);
    }

    // Order sessions by bucket (Needs input, then Working, then Stopped),
    // most-recently-active first within each. Selection is tracked by id
    // across reloads, so re-sorting doesn't move the cursor off a session.
    fn sort_for_display(&mut self) {
        let activity = std::mem::take(&mut self.activity);
        let mut decorated: Vec<(u8, u64, u64, SessionState)> = self
            .sessions
            .drain(..)
            .map(|s| (bucket_rank(bucket_of(&activity, &s)), s.updated_at, s.created_at, s))
            .collect();
        decorated.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| b.2.cmp(&a.2))
        });
        self.sessions = decorated.into_iter().map(|(_, _, _, s)| s).collect();
        self.activity = activity;
        self.regroup_pilots();
    }

    // Pull each pilot session out of its bucket-sorted slot and reinsert it
    // directly after the main it answers, so the list reads as a group ("main"
    // then its indented "↳ pilot"). Mirrors nothing in codex — purely rail's view.
    fn regroup_pilots(&mut self) {
        if self.pilot_to_main.is_empty() {
            return;
        }
        let pilot_ids: std::collections::HashSet<String> =
            self.pilot_to_main.keys().cloned().collect();
        let mut pilots: Vec<SessionState> = Vec::new();
        self.sessions.retain(|s| {
            if pilot_ids.contains(&s.id) {
                pilots.push(s.clone());
                false
            } else {
                true
            }
        });
        for pilot in pilots {
            match self
                .pilot_to_main
                .get(&pilot.id)
                .and_then(|mid| self.sessions.iter().position(|s| &s.id == mid))
            {
                Some(pos) => self.sessions.insert(pos + 1, pilot),
                None => self.sessions.push(pilot), // orphan pilot (main removed)
            }
        }
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

    // Discard the diff cache so the next render repaints the whole screen. Call
    // after anything that blanks the terminal behind our back — a resize, or
    // returning from an attach (which left and re-entered the alt screen).
    fn invalidate_frame(&mut self) {
        self.prev_frame.clear();
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
    // Any fresh keypress clears the previous transient status ("renamed",
    // "detached", errors, confirm prompts) so it fades on your next action. The
    // confirm flows below re-set their own message after this clear, so the two
    // presses of Ctrl+X / Esc still show their prompt in between.
    app.message.clear();

    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('r') {
        if let Some(session) = app.current() {
            app.input = session.title.clone();
            app.mode = Mode::Rename;
            app.stop_confirm = None;
            app.exit_confirm = None;
        }
        return Ok(false);
    }

    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('x') {
        stop_with_confirmation(app)?;
        return Ok(false);
    }

    // Ctrl+D — archive distillation: kick off a background scan of your codex +
    // Claude history; the manager loop launches the codex session once it's ready.
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('d') {
        start_distillation(app);
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
        }
        // SPACE toggles autopilot on the selected session (rail answers it for
        // you via a pilot session). Swallowed here so it never falls through to
        // the "type any key to start a new session" arm below.
        KeyCode::Char(' ') if key.modifiers.is_empty() => {
            app.toggle_autopilot();
        }
        KeyCode::Char(ch) if key.modifiers.is_empty() && !ch.is_control() => {
            app.mode = Mode::New;
            app.input.clear();
            app.input.push(ch);
            app.stop_confirm = None;
            app.exit_confirm = None;
        }
        _ => {}
    }

    Ok(false)
}

// Composer slash commands (Claude-Code-style): typing `/` in the composer opens a
// palette of rail actions, run rail-side instead of being sent to codex.
#[derive(Clone, Copy)]
enum SlashCmd {
    Distill,
    Update,
    Config,
    Help,
    Quit,
}
struct Slash {
    name: &'static str,
    desc: &'static str,
    cmd: SlashCmd,
}
const SLASH_COMMANDS: &[Slash] = &[
    Slash {
        name: "/distill",
        desc: "distill your style + logic from codex/Claude history",
        cmd: SlashCmd::Distill,
    },
    Slash {
        name: "/update",
        desc: "check for and install a newer rail",
        cmd: SlashCmd::Update,
    },
    Slash {
        name: "/config",
        desc: "show config & data locations",
        cmd: SlashCmd::Config,
    },
    Slash {
        name: "/help",
        desc: "show keys & commands",
        cmd: SlashCmd::Help,
    },
    Slash {
        name: "/quit",
        desc: "quit rail (sessions keep running)",
        cmd: SlashCmd::Quit,
    },
];

// Commands whose name starts with the current input (which begins with '/').
fn slash_matches(input: &str) -> Vec<&'static Slash> {
    let q = input.trim();
    SLASH_COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(q))
        .collect()
}

// Run the selected slash command (Enter in the composer while input starts with
// '/'). Returns true to quit the manager.
fn run_slash(app: &mut App) -> Result<bool> {
    let matches = slash_matches(&app.input);
    let picked = matches
        .get(app.slash_sel)
        .or_else(|| matches.first())
        .map(|c| c.cmd);
    let raw = app.input.clone();
    app.mode = Mode::Normal;
    app.input.clear();
    app.slash_sel = 0;
    let Some(cmd) = picked else {
        app.message = format!("unknown command: {}", raw.trim());
        return Ok(false);
    };
    match cmd {
        SlashCmd::Distill => start_distillation(app),
        SlashCmd::Update => start_update(app),
        SlashCmd::Config => {
            app.message = format!(
                "data {} · distill {} · codex {}",
                state::data_dir().display(),
                state::distill_dir().display(),
                state::codex_home_dir().display()
            );
        }
        SlashCmd::Help => {
            app.message =
                "↑↓ move · Enter attach · e new · Space autopilot · / commands · Ctrl+X twice stop · Esc twice quit"
                    .to_string();
        }
        SlashCmd::Quit => return Ok(true),
    }
    Ok(false)
}

// Kick off `rail update` in the background (a curl check + download); the result
// lands on the status line. Never blocks the UI.
fn start_update(app: &mut App) {
    app.message = "checking for updates \u{2026}".to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let msg = match update::newer_available() {
            Some(_) => match update::apply() {
                Ok(tag) => format!("updated to {tag} — restart rail to run it"),
                Err(e) => format!("update failed: {e:#}"),
            },
            None => "already up to date (or GitHub unreachable)".to_string(),
        };
        let _ = tx.send(msg);
    });
    app.update_apply = Some(rx);
}

fn handle_input_key(
    key: KeyEvent,
    app: &mut App,
    terminal: &mut TerminalSession,
    mode: Mode,
) -> Result<bool> {
    // While composing a NEW session, a leading '/' turns the box into a command
    // palette: navigate with ↑↓, run with Enter, complete with Tab.
    let slashing = mode == Mode::New && app.input.starts_with('/');
    if slashing {
        match key.code {
            KeyCode::Up => {
                app.slash_sel = app.slash_sel.saturating_sub(1);
                return Ok(false);
            }
            KeyCode::Down => {
                let n = slash_matches(&app.input).len();
                if n > 0 {
                    app.slash_sel = (app.slash_sel + 1).min(n - 1);
                }
                return Ok(false);
            }
            KeyCode::Tab => {
                let matches = slash_matches(&app.input);
                if let Some(c) = matches.get(app.slash_sel).or_else(|| matches.first()) {
                    app.input = c.name.to_string();
                }
                return Ok(false);
            }
            KeyCode::Enter => return run_slash(app),
            _ => {}
        }
    }
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
            app.slash_sel = 0;
        }
        KeyCode::Enter => {
            submit_input(app, terminal, mode)?;
        }
        // Ctrl+D is a submit alias for a real message, but not while a slash
        // command is being composed (that's handled above).
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL && !slashing => {
            submit_input(app, terminal, mode)?;
        }
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            if !ch.is_control() {
                app.input.push(ch);
                app.slash_sel = 0;
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
                // Write ONLY the manager-owned label file. state.json (which the
                // worker keeps rewriting) is left untouched, so nothing can
                // revert the rename — label.json is authoritative on load.
                let id = current.id.clone();
                match state::write_label(&id, &text, true) {
                    Ok(()) => {
                        if let Some(s) = app.sessions.iter_mut().find(|s| s.id == id) {
                            s.title = text.clone();
                            s.title_pinned = true;
                        }
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
    col: u16,
    app: &mut App,
    terminal: &mut TerminalSession,
) -> Result<bool> {
    let row_index = app
        .rows
        .iter()
        .find_map(|(known_row, index)| (*known_row == row).then_some(*index));

    match kind {
        // A LEFT CLICK on the header's "↑ update available" note runs the update;
        // on a row it selects + attaches.
        MouseEventKind::Down(MouseButton::Left) => {
            if row == 0 {
                if let Some((a, b)) = app.update_click {
                    if (a..b).contains(&col) {
                        start_update(app);
                        return Ok(false);
                    }
                }
            }
            if let Some(index) = row_index {
                app.selected = index;
                attach_current(app, terminal)?;
            }
        }
        // The wheel moves the selection (the view follows it), like ↑↓. Without
        // this the wheel did nothing.
        MouseEventKind::ScrollUp => app.move_prev(),
        MouseEventKind::ScrollDown => app.move_next(),
        // Deliberately ignore Moved: hover-to-select made any mouse twitch jerk
        // the selection to whatever row was under the pointer — which, on a
        // scrolled list, snapped it back to the top.
        _ => {}
    }
    Ok(false)
}

// Number of attaches over which we keep teaching the detach key. codex takes
// over the whole screen with no room for a persistent status bar, and Ctrl+Z's
// usual meaning (suspend the job) actively misleads — so we teach it right
// before handing off. Once was too easy to miss, so we repeat it for the first
// few attaches, then stop for good.
const DETACH_HINT_TIMES: u32 = 10;

// The one thing a new user genuinely cannot discover on their own: once codex
// takes over the screen there is nothing on it that says how to get back. Before
// the first DETACH_HINT_TIMES attaches (tracked in a small counter file) we show
// a short full-screen note with a progress bar that fills as the handoff nears —
// a bar reads as "loading, please wait" far more naturally than counting a bare
// number down. The total duration is overridable via CODEX_RAIL_HINT_MS (the
// tests set it low to stay fast).
fn show_detach_hint() {
    let flag = state::data_dir().join(".detach_hint_count");
    let shown: u32 = fs::read_to_string(&flag)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if shown >= DETACH_HINT_TIMES {
        return;
    }
    let total = Duration::from_millis(
        env::var("CODEX_RAIL_HINT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4000),
    )
    .max(Duration::from_millis(1));
    let frame = Duration::from_millis(40); // ~25fps: smooth without thrashing
    let l1 = "Attaching to codex \u{2026}";
    let l2 = "Press  Ctrl+Z  any time to come back to rail.";
    // Tell the user this note is temporary and counting down, so a first-timer
    // doesn't assume it will interrupt every single attach forever. `shown` is
    // this note's 0-based index, so DETACH_HINT_TIMES - (shown+1) is how many
    // remain after this one.
    let remaining = DETACH_HINT_TIMES.saturating_sub(shown + 1);
    let l3 = match remaining {
        0 => "Last reminder \u{2014} you won't see this again.".to_string(),
        1 => "This reminder will show 1 more time, then stop.".to_string(),
        n => format!("This reminder will show {n} more times, then stop."),
    };
    let style = progress::Style {
        fill: C_ACCENT,
        track: C_FAINT,
    };
    let mut out = io::stdout();
    // Draw the hint in its OWN alternate screen so it never touches the screen
    // codex is using. Leaving the alt buffer restores codex's screen exactly as
    // it was. This is what fixes the 2nd+-attach occlusion: we used to Clear(All)
    // codex's real screen here, but the worker only replays a raw output *tail*
    // on attach (worker.rs send_log_tail), not a full repaint — so a reattached
    // codex, whose recent output is partial updates rather than a whole frame,
    // came back with holes. Own-buffer = codex's screen is never wiped.
    let _ = execute!(out, EnterAlternateScreen, Clear(ClearType::All), Hide);
    let mut last_size = (0u16, 0u16);
    let start = Instant::now();
    loop {
        let elapsed = start.elapsed();
        // fill() clamps, so an overshoot on the final frame just reads as 100%.
        let frac = elapsed.as_secs_f64() / total.as_secs_f64();
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let cy = rows / 2;
        let center = |w: u16| cols.saturating_sub(w) / 2;
        let bar_w = 34u16.min(cols.saturating_sub(6)).max(1);
        // Paint the two static lines once (and again only on resize) so the bar
        // row is the only thing redrawn each frame — no whole-screen clear per
        // frame, hence no flicker as the bar animates.
        if (cols, rows) != last_size {
            let _ = execute!(
                out,
                Clear(ClearType::All),
                MoveTo(center(display_width(l1) as u16), cy.saturating_sub(2)),
                SetForegroundColor(C_DIM),
                Print(l1),
                MoveTo(center(display_width(l2) as u16), cy),
                SetForegroundColor(C_ACCENT),
                SetAttribute(Attribute::Bold),
                Print(l2),
                SetAttribute(Attribute::Reset),
                MoveTo(center(display_width(&l3) as u16), cy + 4),
                SetForegroundColor(C_FAINT),
                Print(&l3),
                ResetColor
            );
            last_size = (cols, rows);
        }
        let bar = progress::render(bar_w, frac, style);
        let _ = execute!(out, MoveTo(center(bar_w), cy + 2), Print(&bar), ResetColor);
        let _ = out.flush();
        if elapsed >= total {
            break;
        }
        thread::sleep(frame.min(total - elapsed));
    }
    // Leave our alternate screen: the terminal restores codex's primary screen
    // untouched (on a reattach, exactly what it showed before the hint), then the
    // worker replays its output tail on top. We deliberately do NOT Clear codex's
    // real screen here — that clear, against a tail-only replay, was the occlusion.
    let _ = execute!(out, ResetColor, Show, LeaveAlternateScreen);
    let _ = out.flush();
    // Count this showing; best-effort — a failure just teaches it again next time.
    let _ = state::ensure_base_dirs();
    let _ = fs::write(&flag, format!("{}\n", shown + 1));
}

fn attach_current(app: &mut App, terminal: &mut TerminalSession) -> Result<()> {
    let Some(mut session) = app.current().cloned() else {
        app.message = "no session".to_string();
        return Ok(());
    };

    // An imported session with a very recently-written rollout may be a codex
    // running in another terminal; resuming it would put a SECOND codex on the
    // same transcript. Confirm with a second Enter before doing that.
    if adopted_maybe_live(&session) {
        let confirmed = app
            .resume_confirm
            .as_ref()
            .map(|(id, at)| id == &session.id && at.elapsed() <= STOP_CONFIRM_WINDOW)
            .unwrap_or(false);
        if !confirmed {
            app.resume_confirm = Some((session.id.clone(), Instant::now()));
            app.message = "this codex session looks active elsewhere — Enter again to resume anyway (starts a 2nd instance)".to_string();
            return Ok(());
        }
        app.resume_confirm = None;
    }

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
    show_detach_hint();
    let result = attach::attach_session(&session);
    terminal.enter_again()?;
    app.invalidate_frame(); // the attach blanked the alt screen — force a full repaint
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

// Recover a codex rollout path for sessions whose worker never captured one —
// a blank session (codex writes no rollout until the first turn), a slow cold
// start that outran the worker's watcher, or a session started by an old rail
// build. Without a rollout path such rows show neither a real status nor a
// message preview (they fall back to the cwd), which is the "right side shows
// the path" bug. We correlate by codex's own session_meta.cwd plus start time
// (UUIDv7 embeds it): the earliest unclaimed rollout in the session's cwd that
// began at/after it was created, within a confidence window. Resolved paths are
// applied to the in-memory session only (used for status + preview); they are
// never written back, so this cannot re-introduce the manager/worker write race.
fn resolve_missing_rollouts(
    sessions: &mut [SessionState],
    cache: &mut HashMap<String, Option<String>>,
) {
    let missing = |s: &SessionState| {
        s.codex_rollout_path
            .as_deref()
            .map(|p| !Path::new(p).exists())
            .unwrap_or(true)
    };

    // Only pay for the sessions-dir scan when there's an undecided session.
    if sessions
        .iter()
        .any(|s| missing(s) && !cache.contains_key(&s.id))
    {
        // Index every rollout once: (cwd, start_secs, path).
        let index: Vec<(String, u64, String)> = state::list_rollout_files()
            .into_iter()
            .filter_map(|path| {
                let (cwd, sid) = state::rollout_head(&path)?;
                let start = state::session_id_start_secs(&sid)?;
                Some((cwd, start, path.to_string_lossy().to_string()))
            })
            .collect();

        // Rollouts already owned by a session that has a path — never reassign.
        let mut claimed: HashSet<String> = sessions
            .iter()
            .filter_map(|s| s.codex_rollout_path.clone())
            .collect();

        // Resolve oldest-created first so an earlier session gets first pick of
        // a shared-cwd rollout before a later one can claim it.
        let mut order: Vec<usize> = (0..sessions.len()).collect();
        order.sort_by_key(|&i| sessions[i].created_at);

        for i in order {
            let (id, cwd, created) = {
                let s = &sessions[i];
                if !missing(s) || cache.contains_key(&s.id) {
                    continue;
                }
                (s.id.clone(), s.cwd.clone(), s.created_at)
            };
            let mut best: Option<(u64, &str)> = None; // (gap, path)
            for (rcwd, start, path) in &index {
                if rcwd != &cwd || claimed.contains(path.as_str()) {
                    continue;
                }
                if *start + 5 < created {
                    continue; // rollout predates the session — not it
                }
                let gap = start.saturating_sub(created);
                if gap > ROLLOUT_MATCH_WINDOW_SECS {
                    continue;
                }
                if best.map(|(g, _)| gap < g).unwrap_or(true) {
                    best = Some((gap, path.as_str()));
                }
            }
            let chosen = best.map(|(_, p)| p.to_string());
            if let Some(p) = &chosen {
                claimed.insert(p.clone());
            }
            cache.insert(id, chosen);
        }
    }

    // Apply memoized resolutions to the in-memory sessions (display only).
    for s in sessions.iter_mut() {
        if missing(s) {
            if let Some(Some(path)) = cache.get(&s.id) {
                s.codex_rollout_path = Some(path.clone());
            }
        }
    }
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
    for s in sessions.iter_mut() {
        if s.title_pinned {
            continue;
        }
        // Prefer codex's own first message from history.jsonl (marker-filtered).
        let from_history = s
            .codex_session_id
            .as_ref()
            .and_then(|sid| firsts.get(sid))
            .map(|m| title_from_message(m))
            .filter(|t| !t.is_empty());
        // If there's nothing in history AND the current title is a junk marker —
        // an old adopted/persisted title like "<command-name>/effort" living in
        // state.json — recover the real first line from the rollout, else fall
        // back to a neutral "codex <id>". Without this, such a title survives
        // forever (sync only ran when history had the id, and these old ids don't).
        let new_title = from_history.or_else(|| {
            if !state::is_synthetic_marker(&s.title) {
                return None;
            }
            s.codex_rollout_path
                .as_ref()
                .and_then(|p| state::rollout_first_user_message(Path::new(p)))
                .map(|m| title_from_message(&m))
                .filter(|t| !t.is_empty())
                .or_else(|| {
                    s.codex_session_id
                        .as_ref()
                        .map(|sid| format!("codex {}", sid.chars().take(8).collect::<String>()))
                })
        });
        if let Some(title) = new_title {
            if title != s.title {
                s.title = title.clone();
                // Sync is unpinned by definition; write the manager-owned label so
                // this never contends with the worker's state.json writes (and it
                // wins over the junk state.json title going forward).
                state::write_label(&s.id, &title, false).ok();
            }
        }
    }
}

// Discover the user's EXISTING codex sessions whose cwd matches the manager's
// launch dir and import them as resumable (exited) rows — so rail manages the
// whole project's codex history, not only sessions it started. Cheap: reads each
// rollout's one-line session_meta header. Skips any codex session already backed
// by a real rail session (`exclude`). Titles come from codex's own history; the
// rows are in-memory until attached, when they resume + persist like any exited
// session (attach -> relaunch_worker -> codex resume <id>).
fn adopt_codex_sessions(
    cwd: &Path,
    exclude: &HashSet<String>,
    since: SystemTime,
    progress: &AdoptProgress,
) -> Vec<SessionState> {
    let target = std::fs::canonicalize(cwd).ok();
    if target.is_none() {
        return Vec::new();
    }
    let codex = env::var("CODEX_RAIL_CODEX").unwrap_or_else(|_| "codex".to_string());
    let firsts = state::codex_first_messages();
    // Stat every rollout, then only READ the ones modified since the last scan
    // (a rescan reads a handful instead of the whole archive). Cheap: stat only.
    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    for p in state::list_rollout_files() {
        let mt = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        if mt >= since {
            files.push((p, mt));
        }
    }
    progress.total.store(files.len(), Ordering::Relaxed);
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (path, mt) in files {
        progress.done.fetch_add(1, Ordering::Relaxed);
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Ok(mut c) = progress.current.lock() {
                *c = name.to_string();
            }
        }
        let Some((rcwd, sid)) = state::rollout_head(&path) else {
            continue;
        };
        let path_str = path.to_string_lossy().to_string();
        if exclude.contains(&sid) || exclude.contains(&path_str) || !seen.insert(sid.clone()) {
            continue;
        }
        // Compare resolved (symlink-free) paths so a trailing slash or a symlinked
        // project dir still lines up; a session whose cwd is gone is skipped.
        match std::fs::canonicalize(&rcwd).ok() {
            Some(r) if Some(&r) == target.as_ref() => {}
            _ => continue,
        }
        let created = state::session_id_start_secs(&sid).unwrap_or(0);
        // Rollout mtime doubles as the row's age and as the live-session signal:
        // a very recently written rollout may be a codex running elsewhere.
        let mtime_secs = mt
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let title = firsts
            .get(&sid)
            .map(|m| title_from_message(m))
            .filter(|t| !t.is_empty())
            .or_else(|| {
                // history.jsonl didn't have it — fall back to the rollout's own
                // first user message so the row still gets a meaningful title.
                state::rollout_first_user_message(Path::new(&path_str))
                    .map(|m| title_from_message(&m))
                    .filter(|t| !t.is_empty())
            })
            .unwrap_or_else(|| format!("codex {}", sid.chars().take(8).collect::<String>()));
        out.push(SessionState {
            id: sid.clone(),
            title,
            cwd: cwd.to_string_lossy().to_string(),
            codex: codex.clone(),
            status: STATUS_EXITED.to_string(),
            worker_pid: None,
            child_pid: None,
            socket: state::socket_path(&sid).to_string_lossy().to_string(),
            created_at: created,
            updated_at: mtime_secs.max(created),
            exit_code: None,
            last_error: None,
            codex_session_id: Some(sid.clone()),
            codex_rollout_path: Some(path_str),
            initial_prompt: None,
            title_pinned: false,
            last_output_at: mtime_secs,
            codex_args: Vec::new(),
            distill_version: None,
            adopted: true,
        });
    }
    out
}

// An imported session whose rollout was written very recently — it may be a
// codex running in another terminal. Resuming it would start a second writer on
// the same transcript, so attach warns + confirms before resuming.
const ADOPT_LIVE_WINDOW_SECS: u64 = 180;
fn adopted_maybe_live(s: &SessionState) -> bool {
    s.adopted && state::now_secs().saturating_sub(s.last_output_at) < ADOPT_LIVE_WINDOW_SECS
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

// Shorten a title for a one-line status message.
fn clip_title(s: &str) -> String {
    let t: String = s.chars().take(30).collect();
    if s.chars().count() > 30 {
        format!("{t}\u{2026}")
    } else {
        t
    }
}

// The newest distilled style file (style-vNNN.md) under distill_dir, if any — the
// pilot reads it to answer in the user's voice. None until the user has distilled.
fn latest_style_file() -> Option<String> {
    let mut best: Option<(u32, String)> = None;
    for entry in std::fs::read_dir(state::distill_dir()).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(v) = name
            .strip_prefix("style-v")
            .and_then(|r| r.strip_suffix(".md"))
            .and_then(|r| r.parse::<u32>().ok())
        {
            if best.as_ref().map(|(bv, _)| v > *bv).unwrap_or(true) {
                best = Some((v, name));
            }
        }
    }
    best.map(|(_, n)| n)
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
    create_session_in(title, initial_prompt, None, Vec::new(), None)
}

// Ctrl+D — archive distillation. Aggregate the user's own past codex messages
// into a small, fully-readable corpus (fast, ~1s), then launch a codex session
// whose cwd is that corpus dir and whose first message tells it to read all of
// it and write a versioned style-vNNN.md. The session runs autonomously
// (workspace-write + a trust override so it never stops for approval) and is
// NOT auto-attached — it shows up like any other session; attach to watch.
fn start_distillation(app: &mut App) {
    if app.distill.is_some() {
        return; // one prep at a time
    }
    // prepare() reads the codex + Claude archives (several seconds), so run it on
    // a worker thread and poll it from the manager loop — the UI stays live.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(distill::prepare());
    });
    app.distill = Some(DistillJob {
        rx,
        started: Instant::now(),
    });
    app.message = "Preparing distillation \u{2026} reading your codex + Claude history".to_string();
}

// Called each manager tick: if the background prep is done, launch the codex
// session; otherwise refresh the elapsed-time status. Returns true if the status
// changed and the screen should repaint.
fn poll_distillation(app: &mut App) -> Result<bool> {
    let Some(job) = &app.distill else {
        return Ok(false);
    };
    match job.rx.try_recv() {
        Err(mpsc::TryRecvError::Empty) => {
            // Still scanning — animate an elapsed-seconds status so it never looks hung.
            let secs = job.started.elapsed().as_secs();
            app.message = format!(
                "Preparing distillation \u{2026} {secs}s (reading your codex + Claude history)"
            );
            Ok(true)
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            app.distill = None;
            app.message = "distill failed: preparer thread stopped".to_string();
            Ok(true)
        }
        Ok(result) => {
            app.distill = None;
            match result {
                Ok(prep) => finish_distillation(app, prep),
                Err(err) => {
                    app.message = format!("distill failed: {err:#}");
                    Ok(true)
                }
            }
        }
    }
}

// Launch the autonomous codex session for a ready corpus. codex runs in the
// corpus dir and only reads/writes there; `-a never -s workspace-write` keeps it
// autonomous, and pre-trusting the dir in codex's config stops its first-run
// "trust this folder?" gate from stalling the unattended run (best-effort). The
// session is tagged with its distill version (drives its list label + "Done"
// status) and is NOT auto-attached — it shows up like any session; attach to watch.
fn finish_distillation(app: &mut App, prep: distill::DistillPrep) -> Result<bool> {
    if prep.messages == 0 {
        app.message = "no history found to distill".to_string();
        return Ok(true);
    }
    let _ = distill::ensure_trusted(&prep.workdir);
    let workdir = prep.workdir.to_string_lossy().to_string();
    let codex_args = vec![
        "-C".to_string(),
        workdir,
        "-s".to_string(),
        "workspace-write".to_string(),
        "-a".to_string(),
        "never".to_string(),
    ];
    let title = format!("[distill v{}]", prep.version);
    let prompt = distill::distill_prompt(&prep);

    match create_session_in(
        &title,
        Some(prompt),
        Some(prep.workdir.clone()),
        codex_args,
        Some(prep.version),
    ) {
        Ok(session) => {
            app.reload()?;
            if let Some(pos) = app.sessions.iter().position(|s| s.id == session.id) {
                app.selected = pos;
            }
            app.message = format!(
                "distilling your style + logic \u{2192} {} ({} sessions: {} codex + {} claude, {} chunks) \u{00b7} Enter to watch",
                prep.output_file,
                prep.sessions,
                prep.codex_sessions,
                prep.claude_sessions,
                prep.chunks.len()
            );
        }
        Err(err) => {
            app.message = format!("distill launch failed: {err:#}");
        }
    }
    Ok(true)
}

// Full form: `cwd_override` pins the session's working directory (else the
// manager's cwd) and `codex_args` are extra flags spliced in before codex's
// prompt/resume args. The distill launcher uses both; ordinary sessions call
// the thin `create_session` wrapper above.
fn create_session_in(
    title: &str,
    initial_prompt: Option<String>,
    cwd_override: Option<PathBuf>,
    codex_args: Vec<String>,
    distill_version: Option<u32>,
) -> Result<SessionState> {
    state::ensure_base_dirs()?;
    let id = state::new_session_id();
    let cwd = match cwd_override {
        Some(p) => p,
        None => env::current_dir().context("current directory")?,
    };
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
        codex_args,
        distill_version,
        adopted: false,
    };
    state::write_state(&session)?;
    // Seed the manager-owned label so the title is authoritative from birth and
    // the worker's state.json writes never define it.
    state::write_label(&id, title, false)?;

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

// Ctrl-X does one of two things depending on whether the session is live:
// a running session is *stopped* (SIGTERM via the worker socket); an
// already-stopped one is *removed* from the list (its job dir is deleted, so
// the row finally goes away). Both need the same double-press confirm. Liveness
// is checked from the actual worker pid, not just the status string, so a
// crashed/zombie worker is treated as stopped and can be cleared.
fn stop_with_confirmation(app: &mut App) -> Result<()> {
    let Some(session) = app.current().cloned() else {
        app.message = "no session".to_string();
        return Ok(());
    };

    let live = session.status == STATUS_STARTING || state::worker_is_running(session.worker_pid);

    let confirmed = app
        .stop_confirm
        .as_ref()
        .map(|(id, at)| id == &session.id && at.elapsed() <= STOP_CONFIRM_WINDOW)
        .unwrap_or(false);

    if !confirmed {
        app.stop_confirm = Some((session.id, Instant::now()));
        app.message = if live {
            "Ctrl+X again to stop this session".to_string()
        } else {
            "Ctrl+X again to remove this stopped session".to_string()
        };
        return Ok(());
    }

    app.stop_confirm = None;
    if live {
        // Ask the worker to shut down cleanly over its socket. If the socket is
        // gone (e.g. $XDG_RUNTIME_DIR was cleared while the worker stayed alive —
        // connect fails with a file error and there's nothing to STOP), fall back
        // to killing the worker by its recorded pid so the row can still be
        // stopped instead of being wedged "running" forever.
        let sent = UnixStream::connect(&session.socket)
            .and_then(|mut s| s.write_all(b"STOP\n").and_then(|_| s.flush()))
            .is_ok();
        if sent {
            app.message = "stop requested".to_string();
        } else if kill_session_pids(&session) {
            app.message = "stopped (socket gone — killed the worker)".to_string();
        } else {
            app.message = "stop failed: worker unreachable and no pid to kill".to_string();
        }
        app.reload()?;
    } else if session.adopted {
        // An imported codex row has no on-disk footprint to delete — "removing" it
        // dismisses it so the rescan stops re-importing it. The codex transcript is
        // kept; without this the row would re-appear on the next scan (the bug).
        if let Some(sid) = &session.codex_session_id {
            let _ = state::dismiss_adopted(sid);
        }
        app.message = "removed from list (codex session kept on disk)".to_string();
        app.invalidate_frame();
        app.reload()?;
    } else {
        match state::remove_session(&session.id) {
            Ok(()) => {
                app.message = "session removed".to_string();
                // The list just shrank; force a clean full repaint so no stale
                // row is left behind below the shortened list.
                app.invalidate_frame();
                app.reload()?;
            }
            Err(err) => {
                app.message = format!("remove failed: {err}");
            }
        }
    }
    Ok(())
}

// Kill a session's worker (and its codex child) by the pids recorded in its own
// state file — the fallback when the worker's socket is gone. SIGTERM lets the
// worker reap its child and clean up; the row flips to Stopped on the next reload.
fn kill_session_pids(session: &SessionState) -> bool {
    let mut any = false;
    for pid in [session.child_pid, session.worker_pid].into_iter().flatten() {
        if pid > 1 {
            // SAFETY: kill(2) with a pid from our own state file; SIGTERM only.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
            any = true;
        }
    }
    any
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
        app.message = "Esc again to quit (sessions keep running)".to_string();
        false
    }
}

// Content is left-aligned with a small margin and capped in width, so a wide
// terminal gets clean right margin rather than columns stretched across a void.
fn content_width(cols: u16) -> usize {
    (cols as usize).min(MAX_CONTENT_COLS)
}

// Build one styled screen row into a String (ANSI included) by queueing into an
// in-memory buffer. Lines carry no MoveTo and no trailing reset — the diff
// emitter positions each row and resets colour after it.
fn styled_line(build: impl FnOnce(&mut Vec<u8>)) -> String {
    let mut buf = Vec::new();
    build(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn put(frame: &mut [String], y: u16, s: String) {
    if let Some(slot) = frame.get_mut(y as usize) {
        *slot = s;
    }
}

// Render by DIFFING against the previous frame: build every screen row into a
// Vec<String>, then rewrite only the rows that actually changed. No whole-screen
// clear each frame (that was the flicker, on every 700ms refresh and keystroke),
// and when nothing changed we write nothing at all — so an idle manager is
// perfectly still. All per-session I/O already happened in refresh_derived, so
// building the frame is pure formatting.
fn render(app: &mut App) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((100, 30));
    let mut frame = vec![String::new(); rows as usize];

    draw_header(&mut frame, cols, app);
    draw_sessions(&mut frame, app, cols, rows);
    draw_input(&mut frame, app, cols, rows);
    draw_hint(&mut frame, app, cols, rows);

    let mut stdout = io::stdout();
    let mut wrote = false;
    if app.prev_frame.len() != frame.len() {
        // First paint or a resize: clear once, force every row to redraw.
        queue!(stdout, ResetColor, Clear(ClearType::All))?;
        app.prev_frame = vec![String::from("\u{1}"); frame.len()]; // sentinel ≠ any real row
        wrote = true;
    }
    for (y, ln) in frame.iter().enumerate() {
        if app.prev_frame[y] != *ln {
            queue!(
                stdout,
                MoveTo(0, y as u16),
                ResetColor,
                Clear(ClearType::UntilNewLine)
            )?;
            stdout.write_all(ln.as_bytes())?;
            queue!(stdout, ResetColor)?;
            wrote = true;
        }
    }
    if wrote {
        queue!(stdout, Hide)?;
        stdout.flush()?;
        app.prev_frame = frame;
    }
    Ok(())
}

fn draw_header(frame: &mut [String], cols: u16, app: &mut App) {
    let cw = content_width(cols);
    let title = "Codex Rail";
    let total = app.sessions.len();
    let count_s = if total == 1 {
        "1 session".to_string()
    } else {
        format!("{total} sessions")
    };

    // Title left, session count right, both inside the capped content width. A
    // pending update shows a quiet amber note just before the count.
    let note = if app.update_available.is_some() {
        "\u{2191} update available  "
    } else {
        ""
    };
    let left_used = 2 + display_width(title);
    let right_w = display_width(note) + display_width(&count_s);
    let gap = cw.saturating_sub(left_used + right_w);
    // Record the note's clickable span (row 0) so a click on it runs the update.
    app.update_click = if note.is_empty() {
        None
    } else {
        let start = (left_used + gap) as u16;
        Some((start, start + display_width(note.trim_end()) as u16))
    };
    put(
        frame,
        0,
        styled_line(|b| {
            let _ = queue!(
                b,
                SetForegroundColor(C_ACCENT),
                SetAttribute(Attribute::Bold),
                Print(format!("  {title}")),
                SetAttribute(Attribute::Reset),
                Print(" ".repeat(gap)),
                SetForegroundColor(C_NEEDS),
                Print(note),
                SetForegroundColor(C_DIM),
                Print(&count_s)
            );
        }),
    );

    // Faint rule under the title.
    let rule_w = cw.saturating_sub(2);
    put(
        frame,
        1,
        styled_line(|b| {
            let _ = queue!(
                b,
                SetForegroundColor(C_FAINT),
                Print(format!("  {}", "─".repeat(rule_w)))
            );
        }),
    );
}

enum DisplayItem {
    Gap,
    Header(Bucket, usize),
    Row(usize),
}

fn draw_sessions(frame: &mut [String], app: &mut App, cols: u16, rows: u16) {
    app.rows.clear();
    let cw = content_width(cols);

    // Size the title column to the actual content (capped); the message-preview
    // column then fills the middle and age sits at the right edge — no dead gap.
    let title_w = session_columns(&app.sessions, cw);

    // List sits between the header rule and the bottom input box. The box top
    // is at rows-4 (see draw_input); leave one blank line above it as a gap.
    let start_y = 3_u16;
    let box_top = rows.saturating_sub(4);
    let last_list_y = box_top.saturating_sub(2);
    let max_rows = (last_list_y + 1).saturating_sub(start_y) as usize;

    if app.sessions.is_empty() {
        // Rest the empty-state line at the vertical middle of the list area, not
        // its top edge — a calm centre is where the eye lands.
        let mid = start_y + (max_rows as u16) / 2;
        put(
            frame,
            mid,
            styled_line(|b| {
                let _ = queue!(
                    b,
                    SetForegroundColor(C_DIM),
                    Print(fit(
                        "  No sessions yet — press e, type your first message, then Enter.",
                        cw
                    ))
                );
            }),
        );
        return;
    }

    // Group session indices by bucket, keeping the global sort order within
    // each. Emit sections in a fixed order but SKIP any that are empty — an
    // empty "Working  0 / none" block is just clutter, so a section appears only
    // when it actually has sessions.
    let mut by_bucket: [Vec<usize>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for (i, s) in app.sessions.iter().enumerate() {
        by_bucket[bucket_slot(bucket_of(&app.activity, s))].push(i);
    }
    let mut items: Vec<DisplayItem> = Vec::new();
    for (slot, b) in [
        Bucket::NeedsInput,
        Bucket::Working,
        Bucket::Done,
        Bucket::Stopped,
    ]
    .into_iter()
    .enumerate()
    {
        if by_bucket[slot].is_empty() {
            continue;
        }
        if !items.is_empty() {
            items.push(DisplayItem::Gap);
        }
        items.push(DisplayItem::Header(b, by_bucket[slot].len()));
        for &i in &by_bucket[slot] {
            items.push(DisplayItem::Row(i));
        }
    }

    // Scroll so the selected row stays visible.
    let sel_pos = items
        .iter()
        .position(|it| matches!(it, DisplayItem::Row(i) if *i == app.selected))
        .unwrap_or(0);
    // When the whole panel fits, float it toward the vertical middle of the list
    // area rather than pinning it under the header — the eye rests at the centre
    // of the screen, not its top edge. When it overflows, fall back to a normal
    // top-anchored scroll that keeps the selected row visible (centring a
    // scrolling list is where it would get jumpy).
    let (render_start_y, offset) = if max_rows == 0 {
        (start_y, 0)
    } else if items.len() <= max_rows {
        (start_y + ((max_rows - items.len()) / 2) as u16, 0)
    } else {
        (start_y, sel_pos.saturating_sub(max_rows.saturating_sub(1)))
    };

    for (visible, item) in items.iter().skip(offset).take(max_rows).enumerate() {
        let y = render_start_y + visible as u16;
        match item {
            DisplayItem::Gap => put(frame, y, String::new()),
            DisplayItem::Header(b, count) => put(frame, y, section_header_line(*b, *count)),
            DisplayItem::Row(index) => {
                let selected = *index == app.selected;
                let s = &app.sessions[*index];
                let bucket = bucket_of(&app.activity, s);
                let preview = app
                    .preview
                    .get(&s.id)
                    .cloned()
                    .unwrap_or_else(|| home_tilde(&s.cwd));
                // Autopilot badge: an active main leads its preview with "⟳ N/cap";
                // a paused one shows why it handed back (so the row says "your turn").
                let preview = match app.autopilot.get(&s.id) {
                    Some(ap) if ap.enabled => {
                        format!("\u{27f3} auto {}/{}  {}", ap.replies, ap.cap, preview)
                    }
                    Some(ap) => format!(
                        "\u{23f8} autopilot \u{2014} {}",
                        ap.last_reason.as_deref().unwrap_or("your turn")
                    ),
                    None => preview,
                };
                put(frame, y, session_row_line(s, bucket, &preview, selected, cw, title_w));
                app.rows.push((y, *index));
            }
        }
    }
    // The selected session's working dir used to sit on its own full-width line
    // here — it overpowered the list. It now rides faintly on the composer's
    // bottom border (see draw_input), where it's available but out of the way.
}

// Section header: coloured glyph + name (bold, bucket colour) + dim count.
// The glyph sits at column 2 so it lines up with each row's own glyph below.
fn section_header_line(b: Bucket, count: usize) -> String {
    styled_line(|buf| {
        let _ = queue!(
            buf,
            SetForegroundColor(bucket_color(b)),
            SetAttribute(Attribute::Bold),
            Print(format!("  {} {}", bucket_glyph(b), bucket_title(b))),
            SetAttribute(Attribute::Reset),
            SetForegroundColor(C_DIM),
            Print(format!("  {count}"))
        );
    })
}

// Title-column width: sized to the widest title actually present (so short
// lists stay tight), capped at ~a third of the row so the message-preview
// column and age still get room. Fixed pieces per row: marker(2) glyph+sp(2)
// gap(2) gap(2) age(4) = 12 columns; the preview fills whatever's left.
fn session_columns(sessions: &[SessionState], cw: usize) -> usize {
    let max_title = sessions
        .iter()
        .map(|s| display_width(&s.title))
        .max()
        .unwrap_or(10);
    let cap = 26.min(cw.saturating_sub(20)).max(6);
    max_title.clamp(6, cap)
}

// One session row, drawn in coloured segments:
//   ▌ {glyph} {title}   {latest codex message}                     {age}
// The selected row gets a terracotta left bar (▌) and a warm background across
// the full content width. The middle is codex's latest line (Claude Code's
// agents panel does the same); a session with no codex message yet (new or
// stopped) falls back to its path so rows stay distinguishable. The segments
// sum to exactly `cw`, so age lands at the right edge. Display-column widths
// keep CJK aligned.
fn session_row_line(
    session: &SessionState,
    bucket: Bucket,
    preview: &str,
    selected: bool,
    cw: usize,
    title_w: usize,
) -> String {
    let mut age: String = format_age(last_activity_secs(session)).chars().take(4).collect();
    while age.chars().count() < 4 {
        age.insert(0, ' '); // right-align in a fixed 4-wide column
    }

    // A distill session carries a distinct "[distill vN]" tag instead of codex's
    // first prompt line, so it's obvious at a glance what it is.
    let label = match session.distill_version {
        Some(v) => format!("[distill v{v}]"),
        None => session.title.clone(),
    };
    let title_s = fit_cols(&label, title_w);
    // Imported codex-history rows (adopted, not started by rail) render dimmer so
    // they read as "resumable history"; one whose rollout was just written shows
    // amber, a hint it may be a codex running elsewhere (attaching warns first).
    let title_color = if selected {
        C_SELTITLE
    } else if adopted_maybe_live(session) {
        C_NEEDS
    } else if session.adopted {
        C_DIM
    } else {
        C_TITLE
    };
    let msg_w = cw.saturating_sub(title_w + 12);
    // Distillation runs for several minutes; while it works, lead the preview with
    // elapsed time + a rough ETA so the long run never reads as stuck.
    let preview_owned;
    let preview: &str = if session.distill_version.is_some() && matches!(bucket, Bucket::Working) {
        let el = state::now_secs().saturating_sub(session.created_at);
        preview_owned = format!(
            "distilling your style + logic \u{2026} {} elapsed \u{00b7} ~15 min typical \u{00b7} {}",
            format_age(el),
            preview
        );
        &preview_owned
    } else {
        preview
    };
    let preview_s = fit_cols(preview, msg_w);

    styled_line(|b| {
        if selected {
            let _ = queue!(b, SetBackgroundColor(C_SEL_BG));
        }
        if selected {
            let _ = queue!(b, SetForegroundColor(C_ACCENT), Print("▌"), Print(" "));
        } else {
            let _ = queue!(b, Print("  "));
        }
        let _ = queue!(
            b,
            SetForegroundColor(bucket_color(bucket)),
            Print(format!("{} ", bucket_glyph(bucket))),
            SetForegroundColor(title_color),
            Print(title_s),
            Print("  "),
            SetForegroundColor(C_DIM),
            Print(preview_s),
            Print("  "),
            SetForegroundColor(if selected { C_DIM } else { C_FAINT }),
            Print(age)
        );
    })
}

fn draw_input(frame: &mut [String], app: &App, cols: u16, rows: u16) {
    let cw = content_width(cols);
    let box_top = rows.saturating_sub(4);
    let box_w = cw.saturating_sub(2); // box occupies columns [2, 2+box_w)
    if box_w < 6 {
        return;
    }
    let inner = box_w - 2; // usable columns between the two side borders

    // Slash-command palette: while composing a new session, a leading '/' turns
    // the box into a command menu — matching commands pop up just above it (like
    // Claude Code), and the highlighted one runs on Enter. It overlays the bottom
    // of the list (a transient overlay; the list redraws when the palette closes).
    if matches!(app.mode, Mode::New) && app.input.starts_with('/') {
        let ms = slash_matches(&app.input);
        let sel = app.slash_sel.min(ms.len().saturating_sub(1));
        for (i, c) in ms.iter().enumerate() {
            let y = box_top.saturating_sub((ms.len() - i) as u16);
            if y < 3 {
                continue;
            }
            let selected = i == sel;
            put(
                frame,
                y,
                styled_line(|b| {
                    if selected {
                        let _ = queue!(b, SetForegroundColor(C_ACCENT), Print("  \u{258c} "));
                    } else {
                        let _ = queue!(b, Print("    "));
                    }
                    let _ = queue!(
                        b,
                        SetForegroundColor(if selected { C_SELTITLE } else { C_TITLE }),
                        Print(format!("{:<9}", c.name)),
                        Print("  "),
                        SetForegroundColor(C_DIM),
                        Print(fit_cols(c.desc, inner.saturating_sub(14)))
                    );
                }),
            );
        }
    }

    // Top border. In compose/rename mode it carries a small mode label as a box
    // title ("╭─ new session ─╮"); otherwise a plain faint border. Transient
    // status and confirm messages live on the bottom hint line now, not here —
    // a quit/stop prompt has no business appearing inside the text box.
    let slashing = matches!(app.mode, Mode::New) && app.input.starts_with('/');
    let label = match app.mode {
        Mode::New if slashing => Some("command"),
        Mode::New => Some("new session"),
        Mode::Rename => Some("rename"),
        Mode::Normal => None,
    };
    let (top, top_color) = match label {
        None => (format!("╭{}╮", "─".repeat(box_w - 2)), C_ACCENT_DIM),
        Some(l) => {
            let seg = format!(" {l} ");
            let seg_w = display_width(&seg).min(box_w.saturating_sub(4));
            let seg_s = fit_cols(&seg, seg_w);
            let rest = (box_w - 2).saturating_sub(1 + seg_w);
            (format!("╭─{}{}╮", seg_s, "─".repeat(rest)), C_ACCENT)
        }
    };
    put(
        frame,
        box_top,
        styled_line(|b| {
            let _ = queue!(b, SetForegroundColor(top_color), Print(format!("  {top}")));
        }),
    );

    // Middle line: "│ ❯ <text>▊ │". In Normal mode this is a dim hint; while
    // typing it's the entered text followed by a block caret — the real terminal
    // cursor is hidden for flicker-free diff rendering, so without a drawn caret
    // the composer looks cursor-less. The mode (new/rename) shows on the border.
    let composing = matches!(app.mode, Mode::New | Mode::Rename);
    let avail = inner.saturating_sub(3);
    put(
        frame,
        box_top + 1,
        styled_line(|b| {
            let _ = queue!(
                b,
                SetForegroundColor(C_ACCENT_DIM),
                Print("  │"),
                SetForegroundColor(C_ACCENT),
                Print(" ❯ ")
            );
            if composing {
                let text_avail = avail.saturating_sub(1); // one column for the caret
                // Show the tail when the input outgrows the box so the end you're
                // typing at stays visible; otherwise show it whole (no padding, so
                // the caret sits right after the text).
                let visible = if display_width(&app.input) <= text_avail {
                    app.input.clone()
                } else {
                    fit_cols_tail(&app.input, text_avail)
                };
                let vis_w = display_width(&visible).min(text_avail);
                let pad = avail.saturating_sub(vis_w + 1);
                let _ = queue!(
                    b,
                    SetForegroundColor(C_TITLE),
                    Print(visible),
                    SetForegroundColor(C_ACCENT),
                    SetAttribute(Attribute::Reverse),
                    Print(" "),
                    SetAttribute(Attribute::Reset),
                    Print(" ".repeat(pad))
                );
            } else {
                let _ = queue!(
                    b,
                    SetForegroundColor(C_DIM),
                    Print(fit_cols(
                        "press e or type to message codex \u{00b7} type / for commands",
                        avail
                    ))
                );
            }
            let _ = queue!(b, SetForegroundColor(C_ACCENT_DIM), Print("│"));
        }),
    );

    // Bottom border — plain, mirroring the top. (The selected session's path used
    // to ride on this border, but sitting on the box chrome read as clutter; it
    // now floats on the spacer row just ABOVE the box instead — see below.)
    put(
        frame,
        box_top + 2,
        styled_line(|b| {
            let _ = queue!(
                b,
                SetForegroundColor(C_ACCENT_DIM),
                Print(format!("  ╰{}╯", "─".repeat(box_w - 2)))
            );
        }),
    );

    // The selected session's working dir rides the blank spacer row just ABOVE
    // the box — faint and right-aligned, aligned to the box's right edge — so
    // "where it runs" stays visible without sitting on the border or taking a
    // full line off the list. Normal mode only; while composing/renaming the row
    // stays clear (the box title already says what mode you're in).
    if matches!(app.mode, Mode::Normal) {
        if let Some(p) = app.sessions.get(app.selected).map(|s| home_tilde(&s.cwd)) {
            let pw = display_width(&p)
                .min((cw as usize).saturating_sub(6))
                .min(40);
            let ps = fit_cols_tail(&p, pw);
            let pad = (cw as usize).saturating_sub(display_width(&ps) + 2);
            put(
                frame,
                box_top.saturating_sub(1),
                styled_line(|b| {
                    let _ = queue!(
                        b,
                        Print(" ".repeat(pad)),
                        SetForegroundColor(C_FAINT),
                        Print(ps)
                    );
                }),
            );
        }
    }
}

// The bottom line: a transient status/confirm message when there is one,
// otherwise mode-aware key hints. Keys are written the way most people expect
// (arrow keys, spelled-out "Ctrl+"/"Esc", "twice" for a double-press) rather
// than terse ^X/w-s notation.
fn draw_hint(frame: &mut [String], app: &App, cols: u16, rows: u16) {
    let (text, color) = if !app.message.is_empty() {
        (app.message.clone(), C_NEEDS) // status/confirm, amber, stands out
    } else {
        let hint = match app.mode {
            Mode::Normal => {
                "↑↓ move · Enter attach · e new · Space autopilot · / commands · Ctrl+R rename · Ctrl+X twice stop · Esc twice quit"
            }
            Mode::New => "Enter start · Esc cancel · / for commands · empty = blank session",
            Mode::Rename => "Enter save · Esc cancel",
        };
        (hint.to_string(), C_DIM)
    };
    // Full width + display-width-aware fit so the "·" (2-byte) separators aren't
    // miscounted and the last word isn't clipped.
    put(
        frame,
        rows.saturating_sub(1),
        styled_line(|b| {
            let _ = queue!(
                b,
                SetForegroundColor(color),
                Print(fit_cols(&format!("  {text}"), cols as usize))
            );
        }),
    );
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
        if let Err(err) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            Hide
        ) {
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
            DisableBracketedPaste,
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
