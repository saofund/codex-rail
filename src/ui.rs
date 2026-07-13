use crate::attach;
use crate::autopilot;
use crate::distill;
use crate::process_tree;
use crate::progress;
use crate::state::{
    self, SessionState, STATUS_EXITED, STATUS_FAILED, STATUS_RUNNING, STATUS_STARTING,
    STATUS_STOPPING,
};
use crate::update;
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
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STOP_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const ADOPT_CONFIRM_WINDOW: Duration = Duration::from_secs(5);
const EXIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const REFRESH_INTERVAL: Duration = Duration::from_millis(700);
const DEFAULT_IMPORT_DAYS: u64 = 7;
const MAX_IMPORT_DAYS: u64 = 3650;
const DAY_SECS: u64 = 24 * 60 * 60;
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

fn days_before(now: SystemTime, days: u64) -> Option<SystemTime> {
    let seconds = days.checked_mul(DAY_SECS)?;
    now.checked_sub(Duration::from_secs(seconds))
}

fn autopilot_phase_timeout_secs() -> u64 {
    env::var("CODEX_RAIL_AUTOPILOT_PHASE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(15 * 60)
}
// Preview tail: enough to contain the last agent_message even when codex writes
// large events (a single message line can be tens of KB).
const ROLLOUT_TAIL_BYTES: u64 = 256 * 1024;
const MAX_COMPOSER_BYTES: usize = 256 * 1024;

// Truecolor palette, tuned for dark terminals. Kept as RGB rather than the
// 8 legacy ANSI colors so it looks consistent regardless of the user's
// terminal theme (the old DarkGrey rendered as near-invisible "black dots").
//
// The brand accent is a warm "terracotta", matching Claude Code's own chrome
// so rail reads as part of the same toolset. Status colours (amber/green/grey)
// stay distinct from the brand accent so they still carry meaning.
const C_ACCENT: Color = Color::Rgb {
    r: 214,
    g: 122,
    b: 90,
}; // terracotta: title, prompt, selection bar
const C_ACCENT_DIM: Color = Color::Rgb {
    r: 150,
    g: 92,
    b: 74,
}; // dim terracotta: input border
const C_TITLE: Color = Color::Rgb {
    r: 230,
    g: 230,
    b: 238,
}; // primary text
const C_SELTITLE: Color = Color::Rgb {
    r: 255,
    g: 255,
    b: 255,
}; // selected primary
const C_DIM: Color = Color::Rgb {
    r: 132,
    g: 140,
    b: 158,
}; // secondary (paths, age, hints)
const C_FAINT: Color = Color::Rgb {
    r: 96,
    g: 102,
    b: 120,
}; // rules
const C_SEL_BG: Color = Color::Rgb {
    r: 42,
    g: 38,
    b: 46,
}; // selected row background (warm-tinted)
const C_HOVER_BG: Color = Color::Rgb {
    r: 35,
    g: 33,
    b: 40,
}; // mouse hover: visible, but quieter than keyboard selection
const C_NEEDS: Color = Color::Rgb {
    r: 236,
    g: 188,
    b: 92,
}; // amber
const C_WORKING: Color = Color::Rgb {
    r: 122,
    g: 208,
    b: 142,
}; // green
const C_DONE: Color = Color::Rgb {
    r: 100,
    g: 200,
    b: 210,
}; // teal: finished task
const C_STOPPED: Color = Color::Rgb {
    r: 140,
    g: 148,
    b: 168,
}; // grey (readable, not "black")

// Cap the content region so age/metadata don't fly to the far edge of an
// ultra-wide terminal. The message-preview column fills the middle, so a wider
// cap just shows more of each codex message (no dead gap) — 120 uses a normal
// wide terminal fully while still reining in a 200-column one.
const MAX_CONTENT_COLS: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    completed_turns: u64,
    partial_record: Vec<u8>,
    discarding_oversized_record: bool,
    caught_up: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = fs::metadata(path).ok()?;
    meta.is_file().then(|| FileStamp {
        path: path.to_path_buf(),
        len: meta.len(),
        modified: meta.modified().ok(),
    })
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
        lc.completed_turns = 0;
        lc.partial_record.clear();
        lc.discarding_oversized_record = false;
        lc.caught_up = false;
    }
    let Ok(mut file) = fs::File::open(path) else {
        return lc.last.unwrap_or(Activity::Waiting);
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < lc.offset {
        // File shrank (rotated/truncated) — rescan from the top.
        lc.offset = 0;
        lc.last = None;
        lc.completed_turns = 0;
        lc.partial_record.clear();
        lc.discarding_oversized_record = false;
        lc.caught_up = false;
    }
    if len > lc.offset && file.seek(SeekFrom::Start(lc.offset)).is_ok() {
        // Bound both one JSON record and work per UI tick. Giant tool events are
        // irrelevant to lifecycle; discard them incrementally to their newline
        // instead of reallocating/re-reading an ever-growing partial line.
        const MAX_RECORD_BYTES: usize = 1024 * 1024;
        const MAX_SCAN_BYTES_PER_TICK: usize = 8 * 1024 * 1024;
        let mut scanned = 0usize;
        let mut buf = [0_u8; 64 * 1024];
        while scanned < MAX_SCAN_BYTES_PER_TICK {
            let want = buf.len().min(MAX_SCAN_BYTES_PER_TICK - scanned);
            let Ok(read) = file.read(&mut buf[..want]) else {
                break;
            };
            if read == 0 {
                break;
            }
            scanned += read;
            lc.offset = lc.offset.saturating_add(read as u64);
            for byte in &buf[..read] {
                if lc.discarding_oversized_record {
                    if *byte == b'\n' {
                        lc.discarding_oversized_record = false;
                    }
                    continue;
                }
                if *byte == b'\n' {
                    observe_lifecycle_record(lc);
                    lc.partial_record.clear();
                } else if lc.partial_record.len() < MAX_RECORD_BYTES {
                    lc.partial_record.push(*byte);
                } else {
                    lc.partial_record.clear();
                    lc.discarding_oversized_record = true;
                }
            }
        }
    }
    lc.caught_up =
        lc.offset >= len && lc.partial_record.is_empty() && !lc.discarding_oversized_record;
    // No marker ever seen (turn hasn't started) → waiting for you.
    if lc.caught_up {
        lc.last.unwrap_or(Activity::Waiting)
    } else {
        Activity::Active
    }
}

fn observe_lifecycle_record(lc: &mut Lifecycle) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&lc.partial_record) else {
        return;
    };
    match value
        .get("payload")
        .and_then(|payload| payload.get("type"))
        .and_then(|kind| kind.as_str())
    {
        Some("task_started") => lc.last = Some(Activity::Active),
        Some("task_complete") => {
            lc.last = Some(Activity::Waiting);
            lc.completed_turns = lc.completed_turns.saturating_add(1);
        }
        Some("turn_aborted" | "thread_rolled_back") => {
            // An interrupted turn is no longer working, but is not a completed
            // reply edge for autopilot.
            lc.last = Some(Activity::Waiting);
        }
        _ => {}
    }
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
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    line.split_whitespace()
        .map(sanitize_terminal_text)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
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
    s.distill_validated
        && s.status == STATUS_EXITED
        && s.worker_token.is_none()
        && s.last_error.is_none()
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
// Never let untrusted session metadata become terminal instructions. Codex
// messages, imported titles, paths, and pasted composer text all eventually
// pass through these helpers before the rendered frame is written to stdout.
// C0/C1 controls can start ANSI/OSC sequences; bidi/zero-width format controls
// can make the UI display something other than the stored text.
fn is_unsafe_terminal_char(c: char) -> bool {
    matches!(c as u32,
        0x0000..=0x001f
        | 0x007f..=0x009f
        | 0x200b..=0x200c
        | 0x200e..=0x200f
        | 0x202a..=0x202e
        | 0x2060..=0x2069
        | 0xfeff
    )
}

fn sanitize_terminal_text(s: &str) -> String {
    s.chars().filter(|c| !is_unsafe_terminal_char(*c)).collect()
}

fn sanitize_composer_text(s: &str) -> String {
    let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !is_unsafe_terminal_char(*c))
        .collect()
}

fn composer_display_text(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\n' => Some('↵'),
            '\t' => Some('⇥'),
            c if is_unsafe_terminal_char(c) => None,
            c => Some(c),
        })
        .collect()
}

fn char_width(c: char) -> usize {
    let u = c as u32;
    if is_unsafe_terminal_char(c) {
        return 0;
    }
    if c == '\u{200d}'
        || (0x0300..=0x036f).contains(&u)
        || (0x1ab0..=0x1aff).contains(&u)
        || (0x1dc0..=0x1dff).contains(&u)
        || (0x20d0..=0x20ff).contains(&u)
        || (0xfe00..=0xfe0f).contains(&u)
        || (0xfe20..=0xfe2f).contains(&u)
        || (0xe0100..=0xe01ef).contains(&u)
        || (0x1f3fb..=0x1f3ff).contains(&u)
    {
        return 0;
    }
    let wide = (0x1100..=0x115F).contains(&u) // Hangul Jamo
        || (0x2600..=0x27bf).contains(&u)      // emoji-presenting symbols
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
    display_clusters(s)
        .into_iter()
        .map(|(_, width)| width)
        .sum()
}

// A compact grapheme approximation for terminal layout: combining/variation/
// skin-tone characters extend the previous cell, and emoji joined with ZWJ
// stay one cluster. This covers the user-visible sequences Rail renders without
// pulling a large Unicode dependency into the static binary.
fn display_clusters(s: &str) -> Vec<(String, usize)> {
    let mut clusters: Vec<(String, usize)> = Vec::new();
    let mut join_next = false;
    for c in s.chars() {
        if is_unsafe_terminal_char(c) {
            continue;
        }
        let width = char_width(c);
        let regional = ('\u{1f1e6}'..='\u{1f1ff}').contains(&c);
        if c == '\u{200d}' {
            if let Some((text, _)) = clusters.last_mut() {
                text.push(c);
                join_next = true;
            }
            continue;
        }
        if width == 0 {
            if let Some((text, _)) = clusters.last_mut() {
                text.push(c);
            }
            continue;
        }
        if regional {
            let joins_flag = clusters.last().is_some_and(|(text, _)| {
                text.chars().count() == 1
                    && text
                        .chars()
                        .next()
                        .is_some_and(|prior| ('\u{1f1e6}'..='\u{1f1ff}').contains(&prior))
            });
            if joins_flag {
                if let Some((text, cluster_width)) = clusters.last_mut() {
                    text.push(c);
                    *cluster_width = 2;
                }
                continue;
            }
        }
        if join_next {
            if let Some((text, cluster_width)) = clusters.last_mut() {
                text.push(c);
                *cluster_width = (*cluster_width).max(width);
            }
            join_next = false;
        } else {
            clusters.push((c.to_string(), width));
        }
    }
    clusters
}

// Truncate to at most `width` display columns and pad with spaces to exactly
// `width` columns (CJK-aware), so every column lines up.
fn fit_cols(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for (cluster, cw) in display_clusters(s) {
        if w + cw > width {
            break;
        }
        out.push_str(&cluster);
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
    let mut kept = Vec::new();
    let mut w = 0;
    for (cluster, cw) in display_clusters(s).into_iter().rev() {
        if w + cw > width.saturating_sub(1) {
            break;
        }
        kept.push(cluster);
        w += cw;
    }
    kept.reverse();
    let tail = kept.concat();
    fit_cols(&format!("…{tail}"), width)
}

fn home_tilde(cwd: &str) -> String {
    match env::var("HOME") {
        Ok(home) if !home.is_empty() => Path::new(cwd)
            .strip_prefix(Path::new(&home))
            .ok()
            .map(|suffix| {
                if suffix.as_os_str().is_empty() {
                    "~".to_string()
                } else {
                    format!("~/{}", suffix.to_string_lossy())
                }
            })
            .unwrap_or_else(|| cwd.to_string()),
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
            if app.refresh_pending_stops() {
                app.reload()?;
            }
            // A distillation is a one-shot job. Once its output exists and the
            // Codex turn is genuinely waiting, stop the worker tree so an idle
            // Codex/MCP stack is not left behind indefinitely.
            if app.stop_completed_distills() {
                app.reload()?;
            }
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
                // Keyboard interaction returns visual focus to the durable
                // selection. Hover is purely pointer-local and must never leave
                // a stale highlight after keyboard-driven scrolling/reordering.
                app.hovered_row = None;
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
            // Preserve multiline code/lists in the actual prompt. The one-line
            // visual composer folds line breaks to a visible marker only.
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
    let cleaned = sanitize_composer_text(text);
    if cleaned.is_empty() {
        return false;
    }
    match app.mode {
        Mode::Normal => {
            app.mode = Mode::New;
            app.input.clear();
            append_composer_text(app, &cleaned);
            app.stop_confirm = None;
            app.exit_confirm = None;
        }
        Mode::New | Mode::Rename => {
            append_composer_text(app, &cleaned);
        }
    }
    true
}

fn append_composer_text(app: &mut App, text: &str) {
    let remaining = MAX_COMPOSER_BYTES.saturating_sub(app.input.len());
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = index + ch.len_utf8();
        if next > remaining {
            break;
        }
        end = next;
    }
    app.input.push_str(&text[..end]);
    if end < text.len() {
        app.message = format!("input capped at {} KiB", MAX_COMPOSER_BYTES / 1024);
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
    // Set when the user tried to attach a maybe-live imported session; a second
    // Enter within the window confirms the resume (which starts a 2nd codex).
    resume_confirm: Option<(String, Instant)>,
    // Highlighted row in the composer's slash-command palette (when the input
    // starts with '/').
    slash_sel: usize,
    rows: Vec<(u16, usize)>,
    // Physical screen row currently under the pointer. This is deliberately
    // separate from `selected`: hover paints only; keyboard navigation, list
    // scrolling, and attach ownership remain driven by `selected`.
    hovered_row: Option<u16>,
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
    preview_stamp: HashMap<String, FileStamp>,
    title_history: HashMap<String, String>,
    title_history_stamp: Option<FileStamp>,
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
    // A manual `/import ...` entered while the automatic scan is still running.
    // Keep the user's explicit request instead of asking them to race/retry it.
    adopt_pending: Option<AdoptRequest>,
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
    // Autopilot control per main session id (Ctrl+A toggles it). Loaded from each
    // session's autopilot.json every reload; the driver advances the reply cycle
    // each tick. `pilot_to_main` is the reverse link (a pilot session's id -> the
    // main it answers) so the list can render pilots grouped under their main.
    autopilot: HashMap<String, autopilot::AutopilotState>,
    // A live flock guard for every autopilot this manager is allowed to drive.
    // Other Rail windows may render the same state but cannot inject messages.
    autopilot_leases: HashMap<String, fs::File>,
    // A persistence failure makes delivery state uncertain. Keep the lease but
    // stop driving until an explicit user toggle successfully writes state.
    autopilot_faults: HashSet<String>,
    pilot_to_main: HashMap<String, String>,
    pending_stops: HashMap<String, PendingStop>,
}

#[derive(Clone, Debug)]
enum AdoptRequest {
    Automatic { since: SystemTime, initial: bool },
    Days(u64),
    Session(String),
}

#[derive(Debug)]
enum ExactImportIssue {
    NotFound,
    AlreadyImported,
    DifferentCwd(String),
    NoConversation,
    Ambiguous(usize),
}

#[derive(Default)]
struct AdoptScanResult {
    sessions: Vec<SessionState>,
    exact_issue: Option<ExactImportIssue>,
    ambiguous: usize,
}

// A background codex-session scan in flight. The startup scan is capped to the
// last seven active days; later automatic scans use the incremental cursor.
// Progress is shared so the UI can draw a real bar of what's being scanned.
struct AdoptJob {
    rx: mpsc::Receiver<AdoptScanResult>,
    progress: Arc<AdoptProgress>,
    scan_start: SystemTime,
    request: AdoptRequest,
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

struct PendingStop {
    requested: Instant,
    escalated: bool,
}

impl App {
    fn load() -> Result<Self> {
        // Migrate private corpus/style artifacts written by older builds before
        // the manager starts doing any other work.
        state::ensure_private_distill_storage()?;
        // Reap rail's own stale worker trees up front (orphaned/exited/duplicate),
        // so leftover codex don't pile up and lock codex's shared ~/.codex sqlite
        // state — the "database is locked" failure. See reap_orphan_workers.
        let reaped = reap_orphan_workers() + reap_abandoned_generations();
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
            hovered_row: None,
            rollout_cache: HashMap::new(),
            activity: HashMap::new(),
            lifecycle: HashMap::new(),
            preview: HashMap::new(),
            preview_stamp: HashMap::new(),
            title_history: HashMap::new(),
            title_history_stamp: None,
            prev_frame: Vec::new(),
            distill: None,
            adopted: HashMap::new(),
            adopt_job: None,
            adopt_pending: None,
            adopt_started: None,
            adopt_since: days_before(SystemTime::now(), DEFAULT_IMPORT_DAYS).unwrap_or(UNIX_EPOCH),
            update_rx: None,
            update_available: None,
            update_apply: None,
            update_click: None,
            autopilot: HashMap::new(),
            autopilot_leases: HashMap::new(),
            autopilot_faults: HashSet::new(),
            pilot_to_main: HashMap::new(),
            pending_stops: HashMap::new(),
        };
        app.sessions = state::load_sessions()?;
        resolve_missing_rollouts(&mut app.sessions, &mut app.rollout_cache);
        app.refresh_titles_from_history();
        app.spawn_adopt_scan(true); // off-thread; adopted rows merge in when it finishes
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
        if reaped > 0 {
            app.message = format!(
                "cleaned up {reaped} stale worker{} (freed codex's local db)",
                if reaped == 1 { "" } else { "s" }
            );
        }
        Ok(app)
    }

    // Load each session's autopilot.json into `autopilot` (keyed by main id) and
    // build the reverse `pilot_to_main` link so pilots render grouped under their
    // main. Cheap: the file exists only for autopiloted sessions.
    fn load_autopilot(&mut self) {
        self.autopilot.clear();
        self.pilot_to_main.clear();
        let controls: Vec<(String, autopilot::AutopilotState)> = self
            .sessions
            .iter()
            .filter_map(|session| {
                autopilot::load(&session.id).map(|control| (session.id.clone(), control))
            })
            .collect();
        let mut pilot_owners: HashMap<String, Vec<String>> = HashMap::new();
        for (main_id, control) in &controls {
            if let Some(pilot_id) = &control.pilot_id {
                pilot_owners
                    .entry(pilot_id.clone())
                    .or_default()
                    .push(main_id.clone());
            }
        }
        let mut enabled = HashSet::new();
        for (main_id, mut control) in controls {
            let corrupt_link = control.pilot_id.as_ref().is_some_and(|pilot_id| {
                pilot_id == &main_id
                    || pilot_owners
                        .get(pilot_id)
                        .is_some_and(|owners| owners.len() != 1)
            });
            if corrupt_link {
                // Two mains must never drive or delete one ambiguous pilot.
                // Drop both claims in this manager's reconciled view and leave
                // the pilot visible for inspection/removal.
                control.enabled = false;
                control.pilot_id = None;
                control.cleanup_pending = false;
                control.enter_phase(autopilot::Phase::Idle);
                control.pending_reply.clear();
                control.last_reason =
                    Some("corrupt autopilot ownership: pilot link is ambiguous".to_string());
                // Reconciliation does not own every affected main's lease, so
                // it must not race a live manager by rewriting these files. An
                // explicit Ctrl+A acquires one main's lease before severing its
                // persisted ambiguous link and starting fresh.
                self.autopilot_faults.insert(main_id.clone());
                self.message =
                    "autopilot ownership conflict disabled; ambiguous pilot kept for inspection"
                        .to_string();
            } else if let Some(pilot_id) = &control.pilot_id {
                self.pilot_to_main.insert(pilot_id.clone(), main_id.clone());
            }
            if control.enabled || control.cleanup_pending {
                enabled.insert(main_id.clone());
            }
            self.autopilot.insert(main_id, control);
        }
        self.autopilot_leases.retain(|id, _| enabled.contains(id));
        for id in enabled {
            if self.autopilot_leases.contains_key(&id) {
                continue;
            }
            if let Ok(Some(lease)) = state::try_acquire_autopilot_lock(&id) {
                self.autopilot_leases.insert(id, lease);
            }
        }

        // `cleanup_pending` is a durable state machine, not a one-process
        // callback. After a manager crash, reacquire the lease and either
        // re-request STOP, finish a dead pilot, or clear a link whose pilot was
        // already removed before the main control write committed.
        let cleanup_jobs: Vec<(String, Option<String>)> = self
            .autopilot
            .iter()
            .filter(|(main_id, control)| {
                control.cleanup_pending && self.autopilot_leases.contains_key(*main_id)
            })
            .map(|(main_id, control)| (main_id.clone(), control.pilot_id.clone()))
            .collect();
        for (main_id, pilot_id) in cleanup_jobs {
            match pilot_id {
                Some(pilot_id) => {
                    if let Some(pilot) = self
                        .sessions
                        .iter()
                        .find(|session| session.id == pilot_id)
                        .cloned()
                    {
                        if !self.pending_stops.contains_key(&pilot_id) {
                            if state::session_worker_is_running(&pilot) {
                                request_session_stop(&pilot).ok();
                            }
                            self.pending_stops.insert(
                                pilot_id,
                                PendingStop {
                                    requested: Instant::now(),
                                    escalated: false,
                                },
                            );
                        }
                    } else if let Some(mut control) = self.autopilot.get(&main_id).cloned() {
                        control.pilot_id = None;
                        control.cleanup_pending = false;
                        if autopilot::save(&main_id, &control).is_ok() {
                            self.autopilot.insert(main_id.clone(), control);
                            self.autopilot_leases.remove(&main_id);
                        }
                    }
                }
                None => {
                    if let Some(mut control) = self.autopilot.get(&main_id).cloned() {
                        control.cleanup_pending = false;
                        if autopilot::save(&main_id, &control).is_ok() {
                            self.autopilot.insert(main_id.clone(), control);
                            self.autopilot_leases.remove(&main_id);
                        }
                    }
                }
            }
        }
    }

    fn acquire_autopilot_lease(&mut self, id: &str) -> Result<bool> {
        if self.autopilot_leases.contains_key(id) {
            return Ok(true);
        }
        let Some(lease) = state::try_acquire_autopilot_lock(id)? else {
            return Ok(false);
        };
        self.autopilot_leases.insert(id.to_string(), lease);
        Ok(true)
    }

    // Ctrl+A on the selected session turns autopilot on/off. On, rail will answer
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
        let on = self.autopilot.get(&id).map(|s| s.enabled).unwrap_or(false);
        match self.acquire_autopilot_lease(&id) {
            Ok(true) => {}
            Ok(false) => {
                self.message =
                    "autopilot is controlled by another Rail window; stop it there first"
                        .to_string();
                return;
            }
            Err(err) => {
                self.message = format!("autopilot lock failed: {err:#}");
                return;
            }
        }
        if on {
            let mut stopped = self.autopilot.get(&id).cloned().unwrap_or_default();
            let pilot_id = stopped.pilot_id.clone();
            stopped.enabled = false;
            stopped.enter_phase(autopilot::Phase::Idle);
            stopped.pending_reply.clear();
            stopped.last_reason = Some("stopped by user".to_string());
            stopped.cleanup_pending = pilot_id.is_some();
            if let Err(err) = autopilot::save(&id, &stopped) {
                self.autopilot_faults.insert(id.clone());
                self.message = format!("couldn't stop autopilot safely: {err:#}");
                return;
            }
            let pilot_cleanup_error = pilot_id.as_deref().and_then(|pid| {
                self.sessions
                    .iter()
                    .find(|s| s.id == pid)
                    .and_then(|pilot| {
                        let result = request_session_stop(pilot);
                        self.pending_stops.insert(
                            pid.to_string(),
                            PendingStop {
                                requested: Instant::now(),
                                escalated: false,
                            },
                        );
                        result.err()
                    })
            });
            self.autopilot.insert(id.clone(), stopped);
            if pilot_id.is_none() {
                self.autopilot_leases.remove(&id);
            }
            self.autopilot_faults.remove(&id);
            self.message = match pilot_cleanup_error {
                Some(err) => format!("autopilot off; pilot cleanup failed: {err:#}"),
                None => format!("autopilot off for \u{201c}{}\u{201d}", clip_title(&title)),
            };
        } else {
            // (re-)enable: fresh cycle, keep any existing pilot to reuse.
            let mut st = autopilot::load(&id).unwrap_or_default();
            if self.autopilot_faults.contains(&id) {
                st.pilot_id = None;
                st.cleanup_pending = false;
                st.pilot_marker.clear();
            }
            if st.cleanup_pending {
                self.message =
                    "pilot cleanup is still pending; wait before re-enabling autopilot".to_string();
                return;
            }
            st.enabled = true;
            st.marker_version = 2;
            st.replies = 0;
            st.enter_phase(autopilot::Phase::Idle);
            st.main_marker.clear();
            st.pending_reply.clear();
            st.last_reason = None;
            st.cleanup_pending = false;
            if let Err(err) = autopilot::save(&id, &st) {
                self.autopilot_leases.remove(&id);
                self.message = format!("couldn't persist autopilot state: {err:#}");
                return;
            }
            self.autopilot_faults.remove(&id);
            self.message = format!(
                "autopilot ON \u{2014} I'll answer \u{201c}{}\u{201d} for you (up to {} replies) \u{00b7} Ctrl+A to stop",
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

    fn session_turn_marker(&self, id: &str) -> Option<String> {
        self.lifecycle
            .get(id)
            .filter(|lifecycle| lifecycle.caught_up && lifecycle.completed_turns > 0)
            .map(|lifecycle| format!("v2:{}", lifecycle.completed_turns))
    }

    fn session_last_agent_message(&self, id: &str) -> Option<String> {
        self.session_rollout(id)
            .and_then(|rollout| autopilot::last_agent_message_full(&rollout))
    }

    fn refresh_pending_stops(&mut self) -> bool {
        let ids: Vec<String> = self.pending_stops.keys().cloned().collect();
        let mut changed = false;
        for id in ids {
            let Some(session) = self
                .sessions
                .iter()
                .find(|session| session.id == id)
                .cloned()
            else {
                self.pending_stops.remove(&id);
                changed = true;
                continue;
            };
            let live = state::session_worker_is_running(&session);
            if !live {
                match state::try_acquire_session_generation_lock(&id) {
                    Ok(Some(clean_guard)) => {
                        let worker_guard = match state::try_acquire_worker_lock(&id) {
                            Ok(Some(guard)) => guard,
                            Ok(None) => {
                                self.message =
                                    "worker lock is still owned; stop not yet verified".to_string();
                                continue;
                            }
                            Err(err) => {
                                self.message = format!("stop verification failed: {err:#}");
                                continue;
                            }
                        };
                        let mut persisted = match state::read_state(&id) {
                            Ok(state) => state,
                            Err(err) => {
                                self.message = format!("stop state verification failed: {err:#}");
                                continue;
                            }
                        };
                        if persisted.worker_token.is_some() {
                            if let Err(err) = recover_generation_before_relaunch(&mut persisted) {
                                self.message = format!(
                                    "worker stopped but generation cleanup remains unverified: {err:#}"
                                );
                                continue;
                            }
                        }
                        if persisted.worker_token.is_some() {
                            self.message =
                                "worker stopped but its generation token is still unresolved"
                                    .to_string();
                            continue;
                        }
                        drop(worker_guard);
                        drop(clean_guard);
                        if self.pilot_to_main.contains_key(&id) {
                            match self.finish_pilot_cleanup(&id) {
                                Ok(()) => {
                                    self.pending_stops.remove(&id);
                                    self.message =
                                        "autopilot pilot stopped and removed".to_string();
                                    changed = true;
                                }
                                Err(err) => {
                                    self.message = format!("pilot cleanup failed: {err:#}");
                                }
                            }
                        } else {
                            self.pending_stops.remove(&id);
                            self.message = format!("stopped {}", clip_title(&session.title));
                            changed = true;
                        }
                    }
                    Ok(None) => {
                        self.message =
                            "worker stopped; guardian is cleaning descendants…".to_string();
                    }
                    Err(err) => {
                        self.message = format!("stop verification failed: {err:#}");
                    }
                }
                continue;
            }

            let Some(pending) = self.pending_stops.get_mut(&id) else {
                continue;
            };
            if !pending.escalated && pending.requested.elapsed() >= Duration::from_secs(8) {
                pending.escalated = true;
                if kill_session_pids_with_signal(&session, libc::SIGKILL) {
                    self.message =
                        "worker did not acknowledge completion; escalated to verified SIGKILL, guardian cleaning…"
                            .to_string();
                } else {
                    self.message =
                        "stop timed out; worker identity could not be proven, session kept"
                            .to_string();
                }
                changed = true;
            } else if pending.escalated && pending.requested.elapsed() >= Duration::from_secs(20) {
                self.pending_stops.remove(&id);
                self.message =
                    "stop remains unverified after escalation; session and token were kept"
                        .to_string();
                changed = true;
            }
        }
        changed
    }

    fn finish_pilot_cleanup(&mut self, pilot_id: &str) -> Result<()> {
        let main_id = self
            .pilot_to_main
            .get(pilot_id)
            .cloned()
            .context("pilot has no durable main link")?;
        state::remove_session(pilot_id).context("remove stopped pilot session")?;
        let mut control = self
            .autopilot
            .get(&main_id)
            .cloned()
            .or_else(|| autopilot::load(&main_id))
            .context("load autopilot control for pilot cleanup")?;
        if control.pilot_id.as_deref() == Some(pilot_id) {
            control.pilot_id = None;
            control.cleanup_pending = false;
            autopilot::save(&main_id, &control)?;
            self.autopilot.insert(main_id.clone(), control);
        }
        self.pilot_to_main.remove(pilot_id);
        self.autopilot_leases.remove(&main_id);
        Ok(())
    }

    fn stop_completed_distills(&mut self) -> bool {
        let completed: Vec<SessionState> = self
            .sessions
            .iter()
            .filter(|s| {
                s.distill_version.is_some()
                    && s.status == STATUS_RUNNING
                    && self.activity.get(&s.id) == Some(&Activity::Waiting)
                    && s.distill_validated
            })
            .cloned()
            .collect();
        let mut requested = false;
        for session in completed {
            match request_session_stop(&session) {
                Ok(()) => requested = true,
                Err(err) => {
                    self.message = format!("completed distill cleanup failed: {err:#}");
                }
            }
        }
        requested
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
            .filter(|(id, st)| {
                st.enabled
                    && self.autopilot_leases.contains_key(*id)
                    && !self.autopilot_faults.contains(*id)
            })
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
        let original = st.clone();
        let mut wrote_intermediate_intent = false;
        if !self.sessions.iter().any(|s| s.id == main_id) {
            if let Some(pid) = st.pilot_id.as_deref() {
                if let Some(pilot) = self.sessions.iter().find(|s| s.id == pid) {
                    let _ = request_session_stop(pilot);
                }
            }
            autopilot::remove(main_id); // main gone
            self.autopilot.remove(main_id);
            self.autopilot_leases.remove(main_id);
            return true;
        }
        if let Some(pilot_id) = st.pilot_id.as_deref() {
            if !self.sessions.iter().any(|session| session.id == pilot_id) {
                st.pilot_id = None;
                st.cleanup_pending = false;
                return self.pause_autopilot(
                    main_id,
                    &mut st,
                    "autopilot pilot session is missing; re-enable to create a new pilot"
                        .to_string(),
                );
            }
        }
        if st.marker_version != 2 {
            st.marker_version = 2;
            return self.pause_autopilot(
                main_id,
                &mut st,
                "autopilot state was upgraded; re-enable it after checking the transcript"
                    .to_string(),
            );
        }
        if st.phase != autopilot::Phase::Idle {
            if st.phase_started_at == 0 {
                return self.pause_autopilot(
                    main_id,
                    &mut st,
                    "autopilot phase has no durable start time; inspect before re-enabling"
                        .to_string(),
                );
            }
            let elapsed = state::now_secs().saturating_sub(st.phase_started_at);
            if elapsed >= autopilot_phase_timeout_secs() {
                return self.pause_autopilot(
                    main_id,
                    &mut st,
                    format!("autopilot phase timed out after {elapsed}s"),
                );
            }
        }
        if st.replies >= st.cap {
            let cap = st.cap;
            return self.pause_autopilot(
                main_id,
                &mut st,
                format!("reached the {cap}-reply limit \u{2014} your turn"),
            );
        }
        let main_msg = self.session_last_agent_message(main_id).unwrap_or_default();
        let main_turn = self.session_turn_marker(main_id).unwrap_or_default();
        let mut changed = false;
        match st.phase {
            autopilot::Phase::Idle => {
                // fire on a NEW completed main turn (and only when idle-waiting)
                if self.session_needs_input(main_id)
                    && !main_msg.is_empty()
                    && !main_turn.is_empty()
                    && main_turn != st.main_marker
                {
                    st.main_marker = main_turn;
                    match st.pilot_id.clone() {
                        None => {
                            let planned_pilot_id = state::new_session_id();
                            st.pilot_id = Some(planned_pilot_id.clone());
                            st.enter_phase(autopilot::Phase::StartingPilot);
                            if let Err(err) = autopilot::save(main_id, &st) {
                                self.autopilot_faults.insert(main_id.to_string());
                                self.message = format!("autopilot intent write failed: {err:#}");
                                self.autopilot.insert(main_id.to_string(), st);
                                return true;
                            }
                            wrote_intermediate_intent = true;
                            match self.spawn_pilot(main_id, &main_msg, &planned_pilot_id) {
                                Some(pid) => {
                                    st.pilot_id = Some(pid);
                                    st.pilot_marker.clear();
                                    st.enter_phase(autopilot::Phase::Generating);
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
                            }
                        }
                        Some(pid) => {
                            // baseline the pilot's current reply, then nudge it
                            st.pilot_marker = self.session_turn_marker(&pid).unwrap_or_default();
                            match self.session_socket(&pid) {
                                Some(sock) => {
                                    st.enter_phase(autopilot::Phase::Nudging);
                                    if let Err(err) = autopilot::save(main_id, &st) {
                                        self.autopilot_faults.insert(main_id.to_string());
                                        self.message =
                                            format!("autopilot intent write failed: {err:#}");
                                        self.autopilot.insert(main_id.to_string(), st);
                                        return true;
                                    }
                                    wrote_intermediate_intent = true;
                                    let prompt = autopilot::continue_prompt(&main_msg);
                                    match autopilot::inject(&sock, &prompt) {
                                        Ok(true) => {
                                            st.enter_phase(autopilot::Phase::Generating);
                                            self.message =
                                                "autopilot: pilot thinking\u{2026}".to_string();
                                            changed = true;
                                        }
                                        Ok(false) => {
                                            st.enter_phase(autopilot::Phase::Idle);
                                            st.main_marker.clear();
                                        }
                                        Err(err) => {
                                            return self.pause_autopilot(
                                                main_id,
                                                &mut st,
                                                format!(
                                                    "pilot nudge failed and may be partial ({err}); check the pilot"
                                                ),
                                            );
                                        }
                                    }
                                }
                                None => st.main_marker.clear(),
                            }
                        }
                    }
                }
            }
            autopilot::Phase::StartingPilot => {
                return self.pause_autopilot(
                    main_id,
                    &mut st,
                    "pilot start was interrupted; check for an unlinked pilot before re-enabling"
                        .to_string(),
                );
            }
            autopilot::Phase::Nudging => {
                return self.pause_autopilot(
                    main_id,
                    &mut st,
                    "pilot nudge was interrupted; inspect the pilot before re-enabling".to_string(),
                );
            }
            autopilot::Phase::Generating => {
                match st.pilot_id.clone() {
                    Some(pid) if self.session_needs_input(&pid) => {
                        let reply = self.session_last_agent_message(&pid).unwrap_or_default();
                        let pilot_turn = self.session_turn_marker(&pid).unwrap_or_default();
                        if !reply.is_empty()
                            && !pilot_turn.is_empty()
                            && pilot_turn != st.pilot_marker
                        {
                            st.pilot_marker = pilot_turn;
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
                                    st.enter_phase(autopilot::Phase::Delivering);
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
                    None => st.enter_phase(autopilot::Phase::Idle),
                }
            }
            autopilot::Phase::Delivering => {
                if !self.session_needs_input(main_id) {
                    // A human/new turn may have made the main busy while the
                    // pilot was thinking. Keep the durable reply pending but do
                    // not type into a working TUI.
                    self.autopilot.insert(main_id.to_string(), st);
                    return false;
                }
                let current_turn = self.session_turn_marker(main_id).unwrap_or_default();
                if current_turn != st.main_marker {
                    st.pending_reply.clear();
                    return self.pause_autopilot(
                        main_id,
                        &mut st,
                        "main session advanced while the pilot was thinking; stale reply was not sent"
                            .to_string(),
                    );
                }
                if let Some(sock) = self.session_socket(main_id) {
                    // Record an intent before touching the PTY. If this manager
                    // crashes in the injection window, the next owner sees
                    // Injecting and hands back rather than sending a duplicate.
                    st.enter_phase(autopilot::Phase::Injecting);
                    if let Err(err) = autopilot::save(main_id, &st) {
                        self.autopilot_faults.insert(main_id.to_string());
                        self.message = format!("autopilot state write failed: {err:#}");
                        self.autopilot.insert(main_id.to_string(), st);
                        return true;
                    }
                    wrote_intermediate_intent = true;
                    match autopilot::inject(&sock, &st.pending_reply) {
                        Ok(true) => {
                            st.replies += 1;
                            st.pending_reply.clear();
                            st.enter_phase(autopilot::Phase::Idle);
                            self.message =
                                format!("autopilot: replied ({}/{})", st.replies, st.cap);
                            changed = true;
                        }
                        Ok(false) => st.enter_phase(autopilot::Phase::Delivering),
                        Err(err) => {
                            return self.pause_autopilot(
                                main_id,
                                &mut st,
                                format!(
                                    "delivery failed and may be partial ({err}); check the transcript"
                                ),
                            );
                        }
                    }
                }
            }
            autopilot::Phase::Injecting => {
                return self.pause_autopilot(
                    main_id,
                    &mut st,
                    "delivery was interrupted; check the transcript before continuing".to_string(),
                );
            }
        }
        if st != original || wrote_intermediate_intent {
            if let Err(err) = autopilot::save(main_id, &st) {
                self.autopilot_faults.insert(main_id.to_string());
                self.message = format!("autopilot state write failed: {err:#}");
                self.autopilot.insert(main_id.to_string(), st);
                return true;
            }
        }
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
        st.enter_phase(autopilot::Phase::Idle);
        st.pending_reply.clear();
        st.last_reason = Some(reason.clone());
        let pilot_id = st.pilot_id.clone();
        st.cleanup_pending = pilot_id.is_some();
        if let Err(err) = autopilot::save(main_id, st) {
            self.autopilot_faults.insert(main_id.to_string());
            self.message = format!("autopilot pause write failed: {err:#}");
            self.autopilot.insert(main_id.to_string(), st.clone());
            return true;
        }
        self.autopilot.insert(main_id.to_string(), st.clone());
        if pilot_id.is_none() {
            self.autopilot_leases.remove(main_id);
        }
        self.autopilot_faults.remove(main_id);
        let cleanup_error = pilot_id.as_deref().and_then(|pid| {
            self.sessions
                .iter()
                .find(|s| s.id == pid)
                .and_then(|pilot| {
                    let result = request_session_stop(pilot);
                    self.pending_stops.insert(
                        pid.to_string(),
                        PendingStop {
                            requested: Instant::now(),
                            escalated: false,
                        },
                    );
                    result.err()
                })
        });
        self.message = match cleanup_error {
            Some(err) => format!("autopilot paused: {reason}; pilot cleanup failed: {err:#}"),
            None => format!("autopilot paused: {reason}"),
        };
        true
    }

    // Create the pilot session for a main: a real, visible codex session (grouped
    // under its main in the list) whose task is to write the user's replies. Runs
    // read-only + autonomous, pinned to the main's cwd, inheriting the user's model.
    fn spawn_pilot(&mut self, main_id: &str, main_msg: &str, planned_id: &str) -> Option<String> {
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
        // The pilot writes a reply — it doesn't need the user's deep-reasoning
        // default, so run it lighter (faster per turn). Overridable.
        let effort = env::var("CODEX_RAIL_PILOT_EFFORT").unwrap_or_else(|_| "medium".to_string());
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
        match create_session_in(
            &title,
            Some(prompt),
            Some(main_cwd),
            codex_args,
            None,
            Some(planned_id.to_string()),
        ) {
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
        self.refresh_titles_from_history();
        // Start a background rescan when idle and the throttle has elapsed (it
        // reads only rollouts modified since the last scan), then merge whatever
        // has already been imported.
        if self.adopt_job.is_none()
            && self
                .adopt_started
                .map(|t| t.elapsed() >= adopt_interval())
                .unwrap_or(false)
        {
            self.spawn_adopt_scan(false);
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
            .map(|s| {
                (
                    s.id.clone(),
                    s.codex_rollout_path.clone(),
                    s.status == STATUS_RUNNING,
                )
            })
            .collect();
        for (id, path, running) in &items {
            match path {
                Some(p) if *running => {
                    let lc = self
                        .lifecycle
                        .entry(id.clone())
                        .or_insert_with(|| Lifecycle {
                            path: String::new(),
                            offset: 0,
                            last: None,
                            completed_turns: 0,
                            partial_record: Vec::new(),
                            discarding_oversized_record: false,
                            caught_up: false,
                        });
                    self.activity.insert(id.clone(), scan_lifecycle(lc, p));
                }
                _ => {
                    self.activity.insert(id.clone(), Activity::Waiting);
                }
            }
            match path.as_deref() {
                Some(p) => {
                    let stamp = file_stamp(Path::new(p));
                    if stamp != self.preview_stamp.get(id).cloned() {
                        match last_agent_message(p) {
                            Some(msg) => {
                                self.preview.insert(id.clone(), msg);
                            }
                            None => {
                                self.preview.remove(id);
                            }
                        }
                        match stamp {
                            Some(stamp) => {
                                self.preview_stamp.insert(id.clone(), stamp);
                            }
                            None => {
                                self.preview_stamp.remove(id);
                            }
                        }
                    }
                }
                None => {
                    self.preview.remove(id);
                    self.preview_stamp.remove(id);
                }
            }
        }
        // Drop cache entries for sessions that no longer exist.
        let ids: std::collections::HashSet<&String> = items.iter().map(|(id, _, _)| id).collect();
        self.activity.retain(|k, _| ids.contains(k));
        self.lifecycle.retain(|k, _| ids.contains(k));
        self.preview.retain(|k, _| ids.contains(k));
        self.preview_stamp.retain(|k, _| ids.contains(k));
        self.rollout_cache.retain(|k, _| ids.contains(k));
    }

    fn refresh_titles_from_history(&mut self) {
        let history = state::codex_home_dir().join("history.jsonl");
        let stamp = file_stamp(&history);
        if stamp != self.title_history_stamp {
            self.title_history = state::codex_first_messages();
            self.title_history_stamp = stamp;
        }
        sync_titles_from_history(&mut self.sessions, &self.title_history);
    }

    // Start the automatic current-cwd scan. Startup begins at the seven-day
    // cutoff; subsequent scans begin at the last completed automatic/day scan.
    fn spawn_adopt_scan(&mut self, initial: bool) {
        self.spawn_adopt_request(AdoptRequest::Automatic {
            since: self.adopt_since,
            initial,
        });
    }

    fn request_import(&mut self, request: AdoptRequest) {
        if let AdoptRequest::Session(id) = &request {
            let already_listed = self
                .sessions
                .iter()
                .any(|session| &session.id == id || session.codex_session_id.as_ref() == Some(id));
            if already_listed {
                self.message = format!("session {} is already in Rail", clip_title(id));
                return;
            }
        }
        if self.adopt_job.is_some() {
            self.adopt_pending = Some(request);
            self.message = "import queued behind the current history scan".to_string();
            return;
        }
        self.spawn_adopt_request(request);
        if let Some(status) = self.adopt_status() {
            self.message = status;
        }
    }

    // Excludes by BOTH codex id and rollout path, since an old Rail session may
    // not have captured its codex id but may still have recovered the exact path.
    // A precise session request intentionally ignores the dismiss list: typing an
    // exact id is an explicit request to restore a previously dismissed row.
    fn spawn_adopt_request(&mut self, request: AdoptRequest) {
        if self.adopt_job.is_some() {
            return;
        }
        let cwd = env::current_dir().unwrap_or_default();
        let mut exclude = if matches!(request, AdoptRequest::Session(_)) {
            HashSet::new()
        } else {
            state::adopt_dismissed()
        };
        for s in &self.sessions {
            exclude.insert(s.id.clone());
            if let Some(id) = &s.codex_session_id {
                exclude.insert(id.clone());
            }
            if let Some(p) = &s.codex_rollout_path {
                exclude.insert(p.clone());
            }
        }
        let scan_start = SystemTime::now();
        let progress = Arc::new(AdoptProgress {
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            current: Mutex::new(String::new()),
        });
        let prog = progress.clone();
        let (tx, rx) = mpsc::channel();
        let scan_request = request.clone();
        thread::spawn(move || {
            let _ = tx.send(adopt_codex_sessions(&cwd, &exclude, &scan_request, &prog));
        });
        self.adopt_job = Some(AdoptJob {
            rx,
            progress,
            scan_start,
            request,
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
        let request = job.request.clone();
        let scan_start = job.scan_start;
        match job.rx.try_recv() {
            Ok(mut found) => {
                if !matches!(request, AdoptRequest::Session(_)) {
                    self.adopt_since = scan_start;
                }
                let imported = found.sessions.len();
                if let AdoptRequest::Session(id) = &request {
                    if imported > 0 {
                        if let Err(err) = state::restore_adopted(id) {
                            found.sessions.clear();
                            found.exact_issue = Some(ExactImportIssue::AlreadyImported);
                            self.message = format!("could not restore imported session: {err:#}");
                        }
                    }
                }
                for s in found.sessions {
                    if let Some(id) = s.codex_session_id.clone() {
                        self.adopted.insert(id, s);
                    }
                }
                self.adopt_job = None;
                if !self
                    .message
                    .starts_with("could not restore imported session")
                {
                    self.message = match request {
                        AdoptRequest::Automatic { initial: true, .. } => {
                            if self.message.is_empty()
                                || self.message.starts_with("importing codex history")
                            {
                                format!(
                                    "auto-imported {imported} chat(s) active in last {DEFAULT_IMPORT_DAYS}d · more: /import 15d or /import <session_id>"
                                )
                            } else {
                                self.message.clone()
                            }
                        }
                        AdoptRequest::Automatic { initial: false, .. } => {
                            if self.message.starts_with("importing codex history") {
                                String::new()
                            } else {
                                self.message.clone()
                            }
                        }
                        AdoptRequest::Days(days) => {
                            let suffix = if found.ambiguous > 0 {
                                format!("; skipped {} ambiguous id(s)", found.ambiguous)
                            } else {
                                String::new()
                            };
                            format!(
                                "imported {imported} additional chat(s) active in last {days}d{suffix}"
                            )
                        }
                        AdoptRequest::Session(id) => match found.exact_issue {
                            None if imported > 0 => format!("imported session {id}"),
                            Some(ExactImportIssue::AlreadyImported) => {
                                format!("session {id} is already in Rail")
                            }
                            Some(ExactImportIssue::DifferentCwd(cwd)) => format!(
                                "session {id} belongs to {cwd}; run Rail from that directory"
                            ),
                            Some(ExactImportIssue::NoConversation) => {
                                format!("session {id} has no genuine conversation to import")
                            }
                            Some(ExactImportIssue::Ambiguous(count)) => format!(
                                "session {id} is ambiguous ({count} rollouts claim it); not imported"
                            ),
                            Some(ExactImportIssue::NotFound) | None => {
                                format!("session {id} was not found in Codex history")
                            }
                        },
                    };
                }
                if let Some(pending) = self.adopt_pending.take() {
                    self.spawn_adopt_request(pending);
                    if let Some(status) = self.adopt_status() {
                        self.message = status;
                    }
                }
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.adopt_job = None;
                self.message = "codex history import failed unexpectedly".to_string();
                if let Some(pending) = self.adopt_pending.take() {
                    self.spawn_adopt_request(pending);
                }
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
        let scope = match &job.request {
            AdoptRequest::Automatic { initial: true, .. } => {
                format!("chats active in last {DEFAULT_IMPORT_DAYS}d")
            }
            AdoptRequest::Automatic { initial: false, .. } => "new chats".to_string(),
            AdoptRequest::Days(days) => format!("chats active in last {days}d"),
            AdoptRequest::Session(id) => format!("session {}", clip_title(id)),
        };
        if total == 0 {
            return Some(format!("importing codex history ({scope}) \u{2026}"));
        }
        // `message` is plain text which is later sanitized.  Embedding a
        // pre-coloured ANSI bar here used to leave visible fragments such as
        // "[38;2;...m" after sanitization; colour is applied only by the final
        // renderer, so keep this intermediate status string escape-free.
        let bar = progress::glyphs(16, done as f64 / total as f64, '\u{2591}');
        let cur = job
            .progress
            .current
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        Some(format!(
            "importing codex history ({scope}) {bar} {done}/{total} \u{00b7} {cur}"
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
                || a.codex_session_id
                    .as_ref()
                    .is_some_and(|c| have.contains(c))
                || a.codex_rollout_path
                    .as_ref()
                    .is_some_and(|p| have.contains(p));
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
            .map(|s| {
                (
                    bucket_rank(bucket_of(&activity, &s)),
                    s.updated_at,
                    s.created_at,
                    s,
                )
            })
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
        self.resume_confirm = None;
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
        KeyCode::Up if key.modifiers.is_empty() => app.move_prev(),
        KeyCode::Down if key.modifiers.is_empty() => app.move_next(),
        KeyCode::Right | KeyCode::Enter if key.modifiers.is_empty() => {
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
        KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
            app.mode = Mode::New;
            app.input.clear();
            app.stop_confirm = None;
            app.exit_confirm = None;
        }
        // Ctrl+A toggles autopilot. Printable keys — including w/s/d/e and a
        // leading space — always enter the composer verbatim.
        KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
            app.toggle_autopilot();
        }
        KeyCode::Char(ch)
            if (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                && !ch.is_control() =>
        {
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
    Import,
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
        name: "/import",
        desc: "older chats: /import 15d or /import <session_id>",
        cmd: SlashCmd::Import,
    },
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
    if q == "/import" || q.starts_with("/import ") {
        return SLASH_COMMANDS
            .iter()
            .filter(|command| command.name == "/import")
            .collect();
    }
    SLASH_COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(q))
        .collect()
}

// Run the selected slash command (Enter in the composer while input starts with
// '/'). Returns true to quit the manager.
fn run_slash(app: &mut App) -> Result<bool> {
    let raw = app.input.clone();
    let trimmed = raw.trim();
    let picked = if trimmed == "/import" || trimmed.starts_with("/import ") {
        Some(SlashCmd::Import)
    } else {
        SLASH_COMMANDS
            .iter()
            .find(|command| command.name == trimmed)
            .map(|command| command.cmd)
    };
    app.mode = Mode::Normal;
    app.input.clear();
    app.slash_sel = 0;
    let Some(cmd) = picked else {
        app.mode = Mode::New;
        app.input = raw;
        return Ok(false);
    };
    match cmd {
        SlashCmd::Import => start_import(app, &raw),
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
                "↑↓ move · Enter attach · Ctrl+N blank · Ctrl+A autopilot · / commands · Ctrl+X twice stop · Esc twice quit"
                    .to_string();
        }
        SlashCmd::Quit => return Ok(true),
    }
    Ok(false)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ImportSpec {
    Days(u64),
    Session(String),
}

const IMPORT_USAGE: &str = "usage: /import 15d or /import <session_id>";

fn looks_like_codex_session_id(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

fn parse_import_spec(input: &str) -> std::result::Result<ImportSpec, &'static str> {
    let mut parts = input.split_whitespace();
    if parts.next() != Some("/import") {
        return Err(IMPORT_USAGE);
    }
    let Some(argument) = parts.next() else {
        return Err(IMPORT_USAGE);
    };
    if parts.next().is_some() {
        return Err(IMPORT_USAGE);
    }
    if let Some(number) = argument
        .strip_suffix('d')
        .or_else(|| argument.strip_suffix('D'))
    {
        let days = number.parse::<u64>().map_err(|_| IMPORT_USAGE)?;
        if days == 0 || days > MAX_IMPORT_DAYS || days.checked_mul(DAY_SECS).is_none() {
            return Err(IMPORT_USAGE);
        }
        return Ok(ImportSpec::Days(days));
    }
    if !looks_like_codex_session_id(argument) || state::validate_session_id(argument).is_err() {
        return Err(IMPORT_USAGE);
    }
    Ok(ImportSpec::Session(argument.to_string()))
}

fn start_import(app: &mut App, input: &str) {
    let request = match parse_import_spec(input) {
        Ok(ImportSpec::Days(days)) => AdoptRequest::Days(days),
        Ok(ImportSpec::Session(id)) => AdoptRequest::Session(id),
        Err(usage) => {
            app.message = usage.to_string();
            return;
        }
    };
    app.request_import(request);
}

// Kick off `rail update` in the background (a curl check + download); the result
// lands on the status line. Never blocks the UI.
fn start_update(app: &mut App) {
    if app.update_apply.is_some() {
        app.message = "an update is already in progress".to_string();
        return;
    }
    app.message = "checking for updates \u{2026}".to_string();
    let (tx, rx) = mpsc::channel();
    let fake = env::var_os("CODEX_RAIL_FAKE_UPDATE").is_some();
    thread::spawn(move || {
        let msg = if fake {
            "already up to date (test mode)".to_string()
        } else {
            match update::newer_available() {
                Some(_) => match update::apply() {
                    Ok(tag) => format!("updated to {tag} — restart rail to run it"),
                    Err(e) => format!("update failed: {e:#}"),
                },
                None => "already up to date (or GitHub unreachable)".to_string(),
            }
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
    let slashing =
        mode == Mode::New && app.input.starts_with('/') && !slash_matches(&app.input).is_empty();
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
                if !app.input.trim().contains(' ') {
                    if let Some(c) = matches.get(app.slash_sel).or_else(|| matches.first()) {
                        app.input = c.name.to_string();
                    }
                }
                return Ok(false);
            }
            KeyCode::Enter => {
                let argument_command = {
                    let input = app.input.trim();
                    input == "/import" || input.starts_with("/import ")
                };
                if argument_command {
                    return run_slash(app);
                }
                let matches = slash_matches(&app.input);
                if let Some(command) = matches.get(app.slash_sel).or_else(|| matches.first()) {
                    app.input = command.name.to_string();
                    return run_slash(app);
                }
            }
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
                let mut encoded = [0_u8; 4];
                append_composer_text(app, ch.encode_utf8(&mut encoded));
                app.slash_sel = 0;
            }
        }
        _ => {}
    }
    Ok(false)
}

fn submit_input(app: &mut App, terminal: &mut TerminalSession, mode: Mode) -> Result<()> {
    let raw = app.input.clone();

    match mode {
        Mode::New => {
            // Empty input → a plain codex session with an auto-numbered title.
            // Any text → that text is both the list title AND codex's first
            // message, started immediately on spawn (so the rollout, and an
            // accurate status, show up within a second or two).
            let (title, prompt) = if raw.trim().is_empty() {
                (default_session_title(&app.sessions), None)
            } else {
                (title_from_message(&raw), Some(raw.clone()))
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
            let text = raw.trim().to_string();
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
        // Hover is visual only. Keep the physical row, not the session index:
        // if status sorting changes underneath a stationary pointer, the row
        // currently under that pointer remains highlighted without mutating the
        // keyboard selection or changing the scroll offset.
        MouseEventKind::Moved => {
            app.hovered_row = row_index.map(|_| row);
        }
        // A LEFT CLICK on the header's "↑ update available" note runs the update;
        // on a row it selects + attaches.
        MouseEventKind::Down(MouseButton::Left) => {
            app.hovered_row = None;
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
        // this the wheel did nothing. Clear the physical-row hover because a
        // scroll can move a different session underneath the stationary pointer.
        MouseEventKind::ScrollUp => {
            app.hovered_row = None;
            app.move_prev();
        }
        MouseEventKind::ScrollDown => {
            app.hovered_row = None;
            app.move_next();
        }
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

    // Rail cannot prove that an imported Codex transcript has no live writer.
    // A stale mtime is not an ownership lease, so every first takeover requires
    // an explicit second Enter. Once Rail relaunches it, `adopted` is cleared in
    // the persisted state and normal reattaches no longer prompt.
    if session.adopted {
        let confirmed = app
            .resume_confirm
            .as_ref()
            .map(|(id, at)| id == &session.id && at.elapsed() <= ADOPT_CONFIRM_WINDOW)
            .unwrap_or(false);
        if !confirmed {
            app.resume_confirm = Some((session.id.clone(), Instant::now()));
            app.message = if adopted_maybe_live(&session) {
                "external session may still be active elsewhere — Enter again to take over (starts another Codex)"
            } else {
                "external session ownership is unknown — Enter again to take over (starts another Codex)"
            }
            .to_string();
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

    // Keep raw mode enabled during the teaching countdown. Ctrl+Z typed as soon
    // as the hint appears is then an ordinary byte (queued for attach/detach),
    // not the terminal driver's VSUSP signal that would suspend the manager.
    show_detach_hint();
    terminal.leave()?;
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
    state::validate_session_id(&session.id)?;
    // Serialize bootstrap/remove across multiple manager processes. The guard is
    // held until a worker is verifiably listening, so a second manager re-checks
    // the winner's state instead of spawning another resume.
    let _init_lock = state::try_acquire_session_init_lock(&session.id)?
        .context("another manager is changing this session; retry resume")?;
    let mut current = state::read_state_optional(&session.id)?;
    if let Some(ref active) = current {
        if state::session_worker_is_running_under_init_lock(active) {
            return wait_for_worker(&session.id, &active.socket, None);
        }
    }

    // Prove that a crashed predecessor's entire token-owned generation is gone
    // before a successor can overwrite its token.  This path matters while the
    // manager remains open: startup recovery alone cannot protect an immediate
    // Enter-to-resume after worker SIGKILL/OOM.
    let old_generation_lock = state::try_acquire_session_generation_lock(&session.id)?
        .context("previous guardian is still cleaning the session; retry resume")?;
    let old_worker_lock = state::try_acquire_worker_lock(&session.id)?
        .context("another worker is claiming this session; retry resume")?;
    if let Some(ref mut abandoned) = current {
        recover_generation_before_relaunch(abandoned)?;
    }

    // Imported sessions have no rail state on disk yet; persist their resume
    // identity once. Existing stopped sessions are deliberately left untouched:
    // with two managers racing, either one rewriting state to "starting" could
    // clobber the winning worker's fresh pid/status. The worker is the sole owner
    // of runtime state and will transition it to running after taking its lock.
    if state::read_state_optional(&session.id)?.is_none() {
        let mut initial = session.clone();
        initial.status = STATUS_STARTING.to_string();
        initial.worker_pid = None;
        initial.child_pid = None;
        initial.exit_code = None;
        initial.last_error = None;
        initial.updated_at = state::now_secs();
        state::write_state(&initial)?;
        if state::read_label(&session.id).is_none() {
            state::write_label(&session.id, &session.title, session.title_pinned)?;
        }
    }

    // The successor guardian takes the generation lease and its child worker
    // takes worker.lock. Keep init.lock held while releasing both and spawning,
    // so competing managers cannot enter the handoff gap.
    drop(old_worker_lock);
    drop(old_generation_lock);

    let child = Command::new(env::current_exe().context("current executable")?)
        .arg("--guardian")
        .arg(&session.id)
        .current_dir(Path::new(&session.cwd))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => {
            record_unclaimed_launch_failure(&session.id, &err.to_string());
            return Err(err).with_context(|| format!("spawn worker for {}", session.title));
        }
    };
    let spawned_pid = child.id();

    thread::spawn(move || {
        let _ = child.wait();
    });

    wait_for_worker(&session.id, &session.socket, Some(spawned_pid))
}

fn recover_generation_before_relaunch(session: &mut SessionState) -> Result<()> {
    let Some(token) = session.worker_token.clone() else {
        if session.worker_lock_protocol
            && matches!(
                session.status.as_str(),
                STATUS_STARTING | STATUS_RUNNING | STATUS_STOPPING
            )
        {
            anyhow::bail!(
                "previous worker died without a recoverable generation token; refusing to start a duplicate Codex"
            );
        }
        return Ok(());
    };

    let report = process_tree::terminate_generation_by_token(
        &token,
        Duration::from_secs(2),
        Duration::from_secs(2),
    );
    session.worker_pid = None;
    session.updated_at = state::now_secs();
    if !report.is_clean() {
        session.status = STATUS_FAILED.to_string();
        session.last_error = Some(format!(
            "abandoned process cleanup incomplete before resume: {} survivor(s), census verified={}; refusing to start another Codex",
            report.survivors, report.verified
        ));
        state::write_state(session)?;
        anyhow::bail!("previous Codex generation cleanup was not verified");
    }

    let had_uncertain_prompt = session.initial_prompt_injecting;
    session.child_pid = None;
    session.worker_token = None;
    session.initial_prompt_injecting = false;
    if had_uncertain_prompt {
        session.initial_prompt = None;
        session.status = STATUS_FAILED.to_string();
        session.last_error = Some(
            "initial prompt delivery was interrupted and may have been submitted; abandoned generation cleaned before resume"
                .to_string(),
        );
    } else if matches!(
        session.status.as_str(),
        STATUS_STARTING | STATUS_RUNNING | STATUS_STOPPING
    ) {
        session.status = STATUS_FAILED.to_string();
        session.last_error = Some(
            "worker died unexpectedly; abandoned process generation cleaned before resume"
                .to_string(),
        );
    }
    state::write_state(session)
}

// Recover a codex rollout path for sessions whose worker never captured one —
// a blank session (codex writes no rollout until the first turn), a slow cold
// start that outran the worker's watcher, or a session started by an old rail
// build. Recovery is exact by Codex session id. A same-cwd/time-window guess is
// not an ownership proof: a blank Rail session and an unrelated direct Codex
// launched seconds later would otherwise be cross-wired, and autopilot could
// answer one transcript through the other's PTY. Sessions lacking an id simply
// wait for their live worker watcher instead of borrowing a plausible rollout.
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

    // A prior no-id miss must not suppress an exact lookup after the worker
    // later captures its Codex session id.
    for session in sessions.iter() {
        if missing(session)
            && session.codex_session_id.is_some()
            && cache.get(&session.id) == Some(&None)
        {
            cache.remove(&session.id);
        }
    }

    // Only pay for the sessions-dir scan when an exact-id lookup is undecided.
    if sessions
        .iter()
        .any(|s| missing(s) && s.codex_session_id.is_some() && !cache.contains_key(&s.id))
    {
        // Index every rollout once by the id from session_meta.
        let index: HashMap<String, String> = state::list_rollout_files()
            .into_iter()
            .filter_map(|path| {
                let (_, sid) = state::rollout_head(&path)?;
                Some((sid, path.to_string_lossy().to_string()))
            })
            .collect();

        // Rollouts already owned by a session that has a path — never reassign.
        let mut claimed: HashSet<String> = sessions
            .iter()
            .filter_map(|s| s.codex_rollout_path.clone())
            .collect();

        for s in sessions.iter() {
            let (id, codex_id) = {
                if !missing(s) || cache.contains_key(&s.id) {
                    continue;
                }
                let Some(codex_id) = s.codex_session_id.clone() else {
                    continue;
                };
                (s.id.clone(), codex_id)
            };
            let chosen = index
                .get(&codex_id)
                .filter(|path| !claimed.contains(path.as_str()))
                .cloned();
            if let Some(p) = &chosen {
                claimed.insert(p.clone());
            }
            cache.insert(id, chosen);
        }
    }

    for session in sessions.iter() {
        if missing(session)
            && session.codex_session_id.is_none()
            && !cache.contains_key(&session.id)
        {
            cache.insert(session.id.clone(), None);
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
fn sync_titles_from_history(sessions: &mut [SessionState], firsts: &HashMap<String, String>) {
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
                if let Ok((authoritative_title, pinned)) =
                    state::sync_label_if_unpinned(&s.id, &title)
                {
                    s.title = authoritative_title;
                    s.title_pinned = pinned;
                }
            }
        }
    }
}

// Discover the user's EXISTING Codex sessions for this working directory and
// import them as resumable rows. Automatic/day scans use rollout mtime as the
// last-activity boundary, then require a genuine user turn so empty bootstrap
// rollouts do not pollute the list. An exact-id scan bypasses time, but not cwd:
// Rail stays a per-project cockpit and tells the user where a foreign id lives.
fn adopt_codex_sessions(
    cwd: &Path,
    exclude: &HashSet<String>,
    request: &AdoptRequest,
    progress: &AdoptProgress,
) -> AdoptScanResult {
    struct Candidate {
        path: PathBuf,
        modified: SystemTime,
        sid: String,
        title: String,
    }

    let mut result = AdoptScanResult::default();
    let target = std::fs::canonicalize(cwd).ok();
    if target.is_none() {
        if matches!(request, AdoptRequest::Session(_)) {
            result.exact_issue = Some(ExactImportIssue::NotFound);
        }
        return result;
    }
    let codex = env::var("CODEX_RAIL_CODEX").unwrap_or_else(|_| "codex".to_string());
    let cutoff = match request {
        AdoptRequest::Automatic { since, .. } => Some(*since),
        AdoptRequest::Days(days) => days_before(SystemTime::now(), *days),
        AdoptRequest::Session(_) => None,
    };
    // Stat every rollout, then only read headers inside the requested activity
    // window. Exact-id imports intentionally inspect all headers.
    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    for p in state::list_rollout_files() {
        let mt = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        if cutoff.is_none_or(|since| mt >= since) {
            files.push((p, mt));
        }
    }
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    progress.total.store(files.len(), Ordering::Relaxed);
    let exact_id = match request {
        AdoptRequest::Session(id) => Some(id.as_str()),
        _ => None,
    };
    let mut exact_matches = 0_usize;
    let mut exact_excluded = false;
    let mut exact_other_cwd = None;
    let mut exact_no_conversation = false;
    let mut candidates: HashMap<String, Vec<Candidate>> = HashMap::new();
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
        if state::validate_session_id(&sid).is_err() {
            continue;
        }
        if let Some(wanted) = exact_id {
            if sid != wanted {
                continue;
            }
            exact_matches += 1;
        }
        let path_str = path.to_string_lossy().to_string();
        if exclude.contains(&sid) || exclude.contains(&path_str) {
            if exact_id.is_some() {
                exact_excluded = true;
            }
            continue;
        }
        // Compare resolved (symlink-free) paths so a trailing slash or a symlinked
        // project dir still lines up; a session whose cwd is gone is skipped.
        match std::fs::canonicalize(&rcwd).ok() {
            Some(r) if Some(&r) == target.as_ref() => {}
            _ => {
                if exact_id.is_some() {
                    exact_other_cwd.get_or_insert(rcwd);
                }
                continue;
            }
        }
        let Some(first_message) = state::rollout_first_user_message(&path) else {
            if exact_id.is_some() {
                exact_no_conversation = true;
            }
            continue;
        };
        let title = title_from_message(&first_message);
        if title.is_empty() {
            if exact_id.is_some() {
                exact_no_conversation = true;
            }
            continue;
        }
        candidates.entry(sid.clone()).or_default().push(Candidate {
            path,
            modified: mt,
            sid,
            title,
        });
    }

    if exact_matches > 1 {
        result.exact_issue = Some(ExactImportIssue::Ambiguous(exact_matches));
        return result;
    }

    for (_sid, mut group) in candidates {
        // Multiple rollouts claiming one id are an identity ambiguity, not a
        // reason to pick whichever directory traversal happened to return first.
        if group.len() != 1 {
            result.ambiguous += 1;
            continue;
        }
        let candidate = group.pop().expect("one candidate");
        let Candidate {
            path,
            modified: mt,
            sid,
            title,
        } = candidate;
        let path_str = path.to_string_lossy().to_string();
        let created = state::session_id_start_secs(&sid).unwrap_or(0);
        // Rollout mtime doubles as the row's age and as the live-session signal:
        // a very recently written rollout may be a codex running elsewhere.
        let mtime_secs = mt
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        result.sessions.push(SessionState {
            id: sid.clone(),
            title,
            cwd: cwd.to_string_lossy().to_string(),
            codex: codex.clone(),
            status: STATUS_EXITED.to_string(),
            worker_pid: None,
            child_pid: None,
            worker_lock_protocol: false,
            worker_token: None,
            socket: state::socket_path(&sid).to_string_lossy().to_string(),
            created_at: created,
            updated_at: mtime_secs.max(created),
            exit_code: None,
            last_error: None,
            codex_session_id: Some(sid.clone()),
            codex_rollout_path: Some(path_str),
            initial_prompt: None,
            initial_prompt_injecting: false,
            title_pinned: false,
            last_output_at: mtime_secs,
            codex_args: Vec::new(),
            distill_version: None,
            distill_expected_markers: Vec::new(),
            distill_expected_user_turns: None,
            distill_corpus_rel: None,
            distill_validated: false,
            adopted: true,
        });
    }
    if exact_id.is_some() && result.sessions.is_empty() {
        result.exact_issue = Some(if exact_matches == 0 {
            ExactImportIssue::NotFound
        } else if exact_excluded {
            ExactImportIssue::AlreadyImported
        } else if let Some(other_cwd) = exact_other_cwd {
            ExactImportIssue::DifferentCwd(other_cwd)
        } else if exact_no_conversation {
            ExactImportIssue::NoConversation
        } else {
            ExactImportIssue::NotFound
        });
    }
    result
}

// Recent activity is a visual warning only. It must never be treated as proof
// that an older imported transcript is safe to take over without confirmation.
const ADOPT_LIVE_WINDOW_SECS: u64 = 180;
fn adopted_maybe_live(s: &SessionState) -> bool {
    s.adopted && state::now_secs().saturating_sub(s.last_output_at) < ADOPT_LIVE_WINDOW_SECS
}

// First non-empty line of a message, length-capped, for use as a list title.
fn title_from_message(msg: &str) -> String {
    let line = msg
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    sanitize_terminal_text(line).chars().take(120).collect()
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
            if distill::validated_style_file(v).is_none() {
                continue;
            }
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
    create_session_in(title, initial_prompt, None, Vec::new(), None, None)
}

struct DistillLaunch {
    version: u32,
    expected_markers: Vec<String>,
    expected_user_turns: usize,
    corpus_rel: String,
    run_lock: fs::File,
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
// "trust this folder?" gate from stalling the unattended run. The
// session is tagged with its distill version (drives its list label + "Done"
// status) and is NOT auto-attached — it shows up like any session; attach to watch.
fn finish_distillation(app: &mut App, mut prep: distill::DistillPrep) -> Result<bool> {
    if prep.messages == 0 {
        app.message = "no history found to distill".to_string();
        return Ok(true);
    }
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
    let run_lock = match prep.run_lock.try_clone() {
        Ok(lock) => lock,
        Err(err) => {
            let cleanup = prep.cleanup_corpus().err();
            app.message = match cleanup {
                Some(cleanup) => format!(
                    "distill launch failed: clone lifetime lock: {err}; private corpus cleanup failed: {cleanup:#}"
                ),
                None => format!("distill launch failed: clone lifetime lock: {err}"),
            };
            return Ok(true);
        }
    };
    let launch = DistillLaunch {
        version: prep.version,
        expected_markers: prep
            .chunks
            .iter()
            .map(|chunk| chunk.marker.clone())
            .collect(),
        expected_user_turns: prep.messages,
        corpus_rel: prep.corpus_rel.clone(),
        run_lock,
    };
    let session_id = state::new_session_id();
    match create_session_in(
        &title,
        Some(prompt),
        Some(prep.workdir.clone()),
        codex_args,
        Some(launch),
        Some(session_id.clone()),
    ) {
        Ok(session) => {
            // The worker/session state now owns removal of this immutable run.
            prep.commit_corpus();
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
            // create_session_in writes state before spawning. If that durable
            // association exists, keep the corpus for retry/removal; otherwise
            // this preparer is the only owner and must erase it now.
            if std::fs::symlink_metadata(state::state_path(&session_id)).is_ok() {
                prep.commit_corpus();
                app.message = format!("distill launch failed: {err:#}");
            } else {
                let cleanup = prep.cleanup_corpus().err();
                app.message = match cleanup {
                    Some(cleanup) => format!(
                        "distill launch failed: {err:#}; private corpus cleanup failed: {cleanup:#}"
                    ),
                    None => format!("distill launch failed: {err:#}"),
                };
            }
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
    distill_launch: Option<DistillLaunch>,
    id_override: Option<String>,
) -> Result<SessionState> {
    state::ensure_base_dirs()?;
    let id = id_override.unwrap_or_else(state::new_session_id);
    state::validate_session_id(&id)?;
    if state::read_state_optional(&id)?.is_some() {
        anyhow::bail!("session id already exists: {id}");
    }
    let cwd = match cwd_override {
        Some(p) => p,
        None => env::current_dir().context("current directory")?,
    };
    // Starting a session from Rail is the user's explicit decision to run Codex
    // in this directory. Persist that trust before spawning so a private prompt
    // can wait for the real composer instead of landing on a first-run trust
    // dialog. Config updates are locked, escaped, private, and atomic.
    distill::ensure_trusted(&cwd).context("trust Rail session directory in Codex config")?;
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
        worker_lock_protocol: false,
        worker_token: None,
        socket: socket.to_string_lossy().to_string(),
        created_at: now,
        updated_at: now,
        exit_code: None,
        last_error: None,
        codex_session_id: None,
        codex_rollout_path: None,
        initial_prompt,
        initial_prompt_injecting: false,
        title_pinned: false,
        last_output_at: 0,
        codex_args,
        distill_version: distill_launch.as_ref().map(|launch| launch.version),
        distill_expected_markers: distill_launch
            .as_ref()
            .map(|launch| launch.expected_markers.clone())
            .unwrap_or_default(),
        distill_expected_user_turns: distill_launch
            .as_ref()
            .map(|launch| launch.expected_user_turns),
        distill_corpus_rel: distill_launch
            .as_ref()
            .map(|launch| launch.corpus_rel.clone()),
        distill_validated: false,
        adopted: false,
    };
    state::write_state(&session)?;
    // Seed the manager-owned label so the title is authoritative from birth and
    // the worker's state.json writes never define it.
    state::write_label(&id, title, false)?;

    let mut worker_command = Command::new(env::current_exe().context("current executable")?);
    worker_command
        .arg("--guardian")
        .arg(&id)
        .current_dir(Path::new(&session.cwd))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(launch) = distill_launch.as_ref() {
        distill::make_run_lock_inheritable(&launch.run_lock)?;
        worker_command.env(
            distill::DISTILL_LOCK_FD_ENV,
            launch.run_lock.as_raw_fd().to_string(),
        );
    }
    let child = worker_command.spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => {
            record_unclaimed_launch_failure(&id, &err.to_string());
            return Err(err).with_context(|| format!("spawn worker for {title}"));
        }
    };
    let spawned_pid = child.id();

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

    wait_for_worker(&session.id, &session.socket, Some(spawned_pid))?;
    Ok(session)
}

fn wait_for_worker(id: &str, socket: &str, expected_pid: Option<u32>) -> Result<()> {
    let path = Path::new(socket);
    for _ in 0..30 {
        match state::read_state(id) {
            Ok(current) if current.status == STATUS_FAILED => {
                anyhow::bail!(
                    "worker failed: {}",
                    current.last_error.as_deref().unwrap_or("unknown error")
                );
            }
            Ok(current) if current.status == STATUS_RUNNING => {
                let claimed = expected_pid
                    .map(|pid| {
                        current.worker_pid == Some(pid)
                            || state::session_worker_is_running_under_init_lock(&current)
                    })
                    .unwrap_or(true);
                if !claimed {
                    std::thread::sleep(Duration::from_millis(80));
                    continue;
                }
                // Existence alone can be a stale inode from a crashed worker. A
                // successful connect proves the claimed worker is listening.
                if UnixStream::connect(path).is_ok() {
                    return Ok(());
                }
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    anyhow::bail!("worker did not open socket {}", path.display())
}

fn record_unclaimed_launch_failure(id: &str, message: &str) {
    if let Ok(mut current) = state::read_state(id) {
        if current.worker_pid.is_none() {
            current.status = STATUS_FAILED.to_string();
            current.last_error = Some(format!("spawn worker: {message}"));
            current.updated_at = state::now_secs();
            let _ = state::write_state(&current);
        }
    }
}

fn request_session_stop(session: &SessionState) -> Result<()> {
    let socket_result = UnixStream::connect(&session.socket).and_then(|mut stream| {
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        stream.write_all(b"STOP\n")?;
        stream.flush()?;
        let response = crate::protocol::read_line(&mut stream)?;
        if response == "STOPPING" {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected STOP acknowledgement: {response:?}"),
            ))
        }
    });
    if socket_result.is_ok() {
        return Ok(());
    }
    state::request_worker_stop(&session.id).with_context(|| {
        format!(
            "worker socket unavailable ({}) and control-file stop failed",
            socket_result.unwrap_err()
        )
    })
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

    let live = state::session_worker_is_running(&session);

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
        match request_session_stop(&session) {
            Ok(()) => {
                app.pending_stops.insert(
                    session.id.clone(),
                    PendingStop {
                        requested: Instant::now(),
                        escalated: false,
                    },
                );
                app.message = "stop accepted; waiting for verified cleanup…".to_string();
            }
            Err(err) => {
                app.message = format!("stop request failed: {err:#}");
            }
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
        if !prepare_autopilot_removal(app, &session.id)? {
            return Ok(());
        }
        let distill_corpus = session.distill_corpus_rel.clone();
        match state::remove_session(&session.id) {
            Ok(()) => {
                let corpus_cleanup = match distill_corpus.as_deref() {
                    Some(corpus) => distill::cleanup_run_corpus(corpus),
                    None => Ok(()),
                };
                app.message = match corpus_cleanup {
                    Ok(()) => "session removed".to_string(),
                    Err(err) => format!("session removed; private corpus cleanup failed: {err:#}"),
                };
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

/// Disable and fully retire a main session's internal pilot before deleting the
/// main's only autopilot link. Deleting autopilot.json first would make a live
/// pilot indistinguishable from an ordinary session and leak it forever.
fn prepare_autopilot_removal(app: &mut App, main_id: &str) -> Result<bool> {
    let Some(mut control) = app.autopilot.get(main_id).cloned() else {
        return Ok(true);
    };
    if !app.acquire_autopilot_lease(main_id)? {
        app.message = "remove blocked: autopilot is controlled by another Rail window".to_string();
        return Ok(false);
    }
    control.enabled = false;
    control.enter_phase(autopilot::Phase::Idle);
    control.pending_reply.clear();
    control.last_reason = Some("main session is being removed".to_string());
    control.cleanup_pending = control.pilot_id.is_some();
    autopilot::save(main_id, &control).context("disable autopilot before removal")?;

    if let Some(pilot_id) = control.pilot_id.as_deref() {
        if let Some(pilot) = app.sessions.iter().find(|s| s.id == pilot_id).cloned() {
            if state::session_worker_is_running(&pilot) {
                let stop = request_session_stop(&pilot);
                app.pending_stops.insert(
                    pilot.id.clone(),
                    PendingStop {
                        requested: Instant::now(),
                        escalated: false,
                    },
                );
                app.message = match stop {
                    Ok(()) => {
                        "stopping linked autopilot pilot; remove the main again once it stops"
                            .to_string()
                    }
                    Err(err) => format!(
                        "pilot stop acknowledgement failed ({err:#}); watchdog will escalate"
                    ),
                };
                app.autopilot.insert(main_id.to_string(), control);
                return Ok(false);
            }
            state::remove_session(&pilot.id).context("remove linked autopilot pilot")?;
        }
    }
    autopilot::remove(main_id);
    app.autopilot.remove(main_id);
    app.autopilot_leases.remove(main_id);
    app.autopilot_faults.remove(main_id);
    Ok(true)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcRef {
    pid: i32,
    start_time: u64,
}

// /proc/<pid>/stat fields after the final `)` begin at field 3 (state). PPID is
// field 4 and starttime is field 22. The starttime makes a pid snapshot immune
// to pid reuse between discovery and signal.
fn proc_identity(pid: i32) -> Option<(i32, u64)> {
    if pid <= 1 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rp = stat.rfind(')')?;
    let fields: Vec<&str> = stat[rp + 1..].split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let start_time = fields.get(19)?.parse().ok()?;
    Some((ppid, start_time))
}

fn proc_ref(pid: i32) -> Option<ProcRef> {
    let (_, start_time) = proc_identity(pid)?;
    Some(ProcRef { pid, start_time })
}

fn same_process(proc: ProcRef) -> bool {
    proc_identity(proc.pid)
        .map(|(_, start)| start == proc.start_time)
        .unwrap_or(false)
}

fn proc_age_secs(proc: ProcRef) -> Option<u64> {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks <= 0 {
        return None;
    }
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let started = proc.start_time as f64 / ticks as f64;
    Some((uptime - started).max(0.0) as u64)
}

// Kill a session's worker AND its codex tree after a strong cmdline/data-dir
// identity check. The start-time snapshot is rechecked immediately before every
// signal so a recycled pid can never inherit the old session's SIGTERM.
fn kill_session_pids_with_signal(session: &SessionState, signal: libc::c_int) -> bool {
    if !state::worker_matches_session(session.worker_pid, &session.id) {
        return false;
    }
    let Some(worker_pid) = session.worker_pid.and_then(state::checked_pid) else {
        return false;
    };
    let Some(worker) = proc_ref(worker_pid) else {
        return false;
    };
    if !state::worker_matches_session(session.worker_pid, &session.id) || !same_process(worker) {
        return false;
    }
    // Derive the codex descendants from the verified worker instead of trusting
    // persisted child_pid/PGID values that may be stale or malformed.
    kill_worker_tree_signal(worker, signal)
}

// Direct children of `pid`, read from /proc PPid. Used to find a worker's codex
// launcher so its whole group can be killed.
fn proc_children_of(worker: ProcRef) -> Vec<ProcRef> {
    let mut kids = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return kids;
    };
    for e in rd.flatten() {
        let Some(cpid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if let Some((ppid, start_time)) = proc_identity(cpid) {
            if ppid == worker.pid {
                kids.push(ProcRef {
                    pid: cpid,
                    start_time,
                });
            }
        }
    }
    kids
}

// Kill a rail worker's whole codex tree, then the worker itself. codex's launcher
// child runs the real codex (+ sub-agents) in its own process group, so -pgid on
// that child takes the subtree; then SIGTERM the worker. SIGTERM only.
fn kill_worker_tree(worker: ProcRef) -> bool {
    kill_worker_tree_signal(worker, libc::SIGTERM)
}

fn kill_worker_tree_signal(worker: ProcRef, signal: libc::c_int) -> bool {
    if !same_process(worker) {
        return false;
    }
    let children = proc_children_of(worker);
    unsafe {
        let my = libc::getpgrp();
        for child in children {
            if proc_identity(child.pid) != Some((worker.pid, child.start_time)) {
                continue;
            }
            let pgid = libc::getpgid(child.pid);
            // Only broadcast to a group led by this exact child. Otherwise signal
            // the verified child pid alone rather than risk an unrelated group.
            if pgid == child.pid
                && pgid != my
                && proc_identity(child.pid) == Some((worker.pid, child.start_time))
            {
                libc::kill(-pgid, signal);
            }
            if proc_identity(child.pid) == Some((worker.pid, child.start_time)) {
                libc::kill(child.pid, signal);
            }
        }
        if same_process(worker) {
            libc::kill(worker.pid, signal);
            return true;
        }
    }
    false
}

// True only when the `rail --worker` at `pid` belongs to THIS manager's data dir,
// determined from its environ (the data dir is XDG_DATA_HOME/codex-rail, else
// HOME/.local/share/codex-rail — the same resolution as state::data_dir). Any
// unreadable environ or mismatch returns false, so the reaper NEVER touches a
// worker it can't prove is its own (a different install, or a test harness).
fn worker_in_my_data_dir(pid: i32, my_data: &Path) -> bool {
    let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    let mut xdg: Option<String> = None;
    let mut home: Option<String> = None;
    for kv in environ.split(|b| *b == 0) {
        if let Some(rest) = kv.strip_prefix(b"XDG_DATA_HOME=") {
            xdg = Some(String::from_utf8_lossy(rest).into_owned());
        } else if let Some(rest) = kv.strip_prefix(b"HOME=") {
            home = Some(String::from_utf8_lossy(rest).into_owned());
        }
    }
    let their_data = match xdg.filter(|x| !x.is_empty()) {
        Some(x) => PathBuf::from(x).join("codex-rail"),
        None => match home.filter(|h| !h.is_empty()) {
            Some(h) => PathBuf::from(h).join(".local/share").join("codex-rail"),
            None => return false,
        },
    };
    their_data == *my_data
}

fn worker_executable_matches_manager(pid: i32, manager_exe: &Path) -> bool {
    let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    let manager_name = manager_exe.file_name().and_then(|n| n.to_str());
    let name_ok = exe
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            let n = n.strip_suffix(" (deleted)").unwrap_or(n);
            Some(n) == manager_name || n.starts_with(".nfs")
        })
        .unwrap_or(false);
    name_ok && exe.parent() == manager_exe.parent()
}

fn worker_candidate_still_matches(
    proc: ProcRef,
    id: &str,
    my_data: &Path,
    manager_exe: &Path,
) -> bool {
    if !same_process(proc)
        || !worker_executable_matches_manager(proc.pid, manager_exe)
        || !worker_in_my_data_dir(proc.pid, my_data)
    {
        return false;
    }
    let Ok(cmd) = std::fs::read(format!("/proc/{}/cmdline", proc.pid)) else {
        return false;
    };
    let parts: Vec<&[u8]> = cmd.split(|b| *b == 0).filter(|p| !p.is_empty()).collect();
    parts.len() == 3 && parts[1] == b"--worker" && parts[2] == id.as_bytes()
}

// Reap rail's OWN stale worker processes so leftover codex don't pile up and lock
// codex's shared ~/.codex sqlite state (the "database is locked" failure). A worker
// is stale when its session dir is gone (removed), its session already
// exited/failed, or it's a DUPLICATE for a session whose current worker is a
// different pid. Each stale worker is killed WITH its codex tree. Never touches the
// live worker of a running session, and only ever signals rail's own `--worker`
// processes (found by cmdline) — never a user's direct codex or a codex sub-agent.
fn reap_orphan_workers() -> usize {
    let me = std::process::id() as i32;
    let my_data = state::data_dir();
    let Ok(manager_exe) = env::current_exe() else {
        return 0;
    };
    let mut by_id: std::collections::HashMap<String, Vec<ProcRef>> =
        std::collections::HashMap::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let parts: Vec<String> = cmd
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        // argv[0] may be NFS silly-renamed to `.nfs…` after a self-update, so its
        // basename is not stable. Require rail's exact hidden-worker argv shape,
        // the same executable directory, and the data-dir boundary. A random
        // same-user process merely containing `--worker` must never be signalled.
        if parts.len() != 3 || parts.get(1).map(String::as_str) != Some("--worker") {
            continue;
        }
        if !worker_executable_matches_manager(pid, &manager_exe) {
            continue;
        }
        // SAFETY-CRITICAL: only ever consider workers that belong to THIS manager's
        // data dir (checked via their environ). Without this, a manager with a
        // different/isolated XDG_DATA_HOME (a test harness, a second install) would
        // see every OTHER manager's workers as "no jobs dir here" and reap them —
        // killing unrelated live sessions. Skip anything we can't prove is ours.
        if !worker_in_my_data_dir(pid, &my_data) {
            continue;
        }
        let Some(proc) = proc_ref(pid) else {
            continue;
        };
        if let Some(id) = parts.get(2) {
            if state::validate_session_id(id).is_ok() {
                by_id.entry(id.clone()).or_default().push(proc);
            }
        }
    }
    let mut reaped = 0;
    for (id, pids) in by_id {
        // A relaunching manager holds init.lock from bootstrap through socket
        // readiness. Never classify that in-between worker from the old exited
        // state. Locks live in jobs/.locks (outside removable session dirs), so
        // the inode remains stable through remove/relaunch races.
        let _init_lock = match state::try_acquire_session_init_lock(&id) {
            Ok(Some(lock)) => lock,
            Ok(None) => continue,
            Err(err) => {
                eprintln!("skip stale-worker check for {id}: {err:#}");
                continue;
            }
        };
        let _generation_lock = match state::try_acquire_session_generation_lock(&id) {
            Ok(Some(lock)) => lock,
            Ok(None) => continue,
            Err(err) => {
                eprintln!("skip stale-worker generation check for {id}: {err:#}");
                continue;
            }
        };
        // A current worker holds this for its whole lifetime. If busy, the
        // process may be in the tiny claim window before state.json changes;
        // never infer staleness from the previous exited snapshot. If acquired,
        // keep it through classification+kill so no worker can start mid-check.
        let _worker_lock = match state::try_acquire_worker_lock(&id) {
            Ok(Some(lock)) => lock,
            // A busy lifetime lock is stronger evidence of a live worker than a
            // transient missing/stale NFS state file is evidence of an orphan.
            // Fail closed and leave it alone; a later startup can retry.
            Ok(None) => continue,
            Err(err) => {
                eprintln!("skip stale-worker check for {id}: {err:#}");
                continue;
            }
        };
        let st = match state::read_state_optional(&id) {
            Ok(st) => st,
            Err(err) => {
                // Fail closed: a transient NFS/permission/JSON error is not proof
                // that the worker is orphaned, so never turn uncertainty into kill.
                eprintln!("skip stale-worker check for {id}: {err:#}");
                continue;
            }
        };
        let no_state = st.is_none();
        let exited = st
            .as_ref()
            .map(|s| matches!(s.status.as_str(), STATUS_EXITED | STATUS_FAILED))
            .unwrap_or(false);
        let starting_timed_out = st
            .as_ref()
            .map(|s| {
                s.status == STATUS_STARTING
                    && s.worker_pid.is_none()
                    && state::now_secs().saturating_sub(s.updated_at) > 15
            })
            .unwrap_or(false);
        let cur = st.as_ref().and_then(|s| {
            if state::worker_matches_session(s.worker_pid, &id) {
                s.worker_pid.and_then(state::checked_pid)
            } else {
                None
            }
        });
        for proc in pids {
            if !exited && cur == Some(proc.pid) {
                continue; // the live session's current worker — keep it
            }
            if proc_age_secs(proc).map(|age| age <= 15).unwrap_or(true) {
                continue; // claim/relaunch grace; uncertainty is never stale proof
            }
            let proven_stale = no_state
                || exited
                || starting_timed_out
                || cur.is_some_and(|owner| owner != proc.pid);
            if proven_stale
                && worker_candidate_still_matches(proc, &id, &my_data, &manager_exe)
                && kill_worker_tree(proc)
            {
                reaped += 1;
            }
        }
    }
    reaped
}

// A worker can itself be SIGKILLed or OOM-killed, bypassing RunGuard::drop.
// Its per-run token survives in state.json and is inherited by every Codex/MCP
// descendant, so a later manager can still close that ownership boundary. Both
// locks are held across classification, census, signalling, and the state write:
// a concurrent resume can neither be mistaken for the abandoned generation nor
// start while its predecessor is being cleaned.
fn reap_abandoned_generations() -> usize {
    // Read raw persisted states. `load_sessions()` intentionally reconciles a
    // dead worker to an in-memory `exited` row, which would hide the exact
    // running/stopping snapshot that tells us crash cleanup is required here.
    let entries = match fs::read_dir(state::jobs_dir()) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("skip abandoned-generation recovery: {err:#}");
            return 0;
        }
    };
    let sessions: Vec<SessionState> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter(|id| state::validate_session_id(id).is_ok())
        .filter_map(|id| state::read_state_optional(&id).ok().flatten())
        .collect();
    let mut recovered = 0;
    for snapshot in sessions {
        let suspicious = matches!(
            snapshot.status.as_str(),
            STATUS_STARTING | STATUS_RUNNING | STATUS_STOPPING
        ) || (snapshot.status == STATUS_FAILED && snapshot.worker_token.is_some());
        if !suspicious || !snapshot.worker_lock_protocol {
            continue;
        }
        let Some(token) = snapshot.worker_token.as_deref() else {
            continue;
        };
        let _init_lock = match state::try_acquire_session_init_lock(&snapshot.id) {
            Ok(Some(lock)) => lock,
            Ok(None) => continue,
            Err(err) => {
                eprintln!("skip abandoned generation {}: {err:#}", snapshot.id);
                continue;
            }
        };
        let _generation_lock = match state::try_acquire_session_generation_lock(&snapshot.id) {
            Ok(Some(lock)) => lock,
            Ok(None) => continue,
            Err(err) => {
                eprintln!("skip abandoned generation {}: {err:#}", snapshot.id);
                continue;
            }
        };
        let _worker_lock = match state::try_acquire_worker_lock(&snapshot.id) {
            Ok(Some(lock)) => lock,
            Ok(None) => continue,
            Err(err) => {
                eprintln!("skip abandoned generation {}: {err:#}", snapshot.id);
                continue;
            }
        };
        let Ok(Some(mut current)) = state::read_state_optional(&snapshot.id) else {
            continue;
        };
        if current.worker_token.as_deref() != Some(token)
            || !current.worker_lock_protocol
            || !matches!(
                current.status.as_str(),
                STATUS_STARTING | STATUS_RUNNING | STATUS_STOPPING | STATUS_FAILED
            )
        {
            continue;
        }

        let report = process_tree::terminate_generation_by_token(
            token,
            Duration::from_secs(2),
            Duration::from_secs(2),
        );
        current.worker_pid = None;
        current.child_pid = None;
        current.updated_at = state::now_secs();
        if report.is_clean() {
            if report.term_signals + report.kill_signals > 0 {
                recovered += 1;
            }
            current.status = STATUS_FAILED.to_string();
            current.worker_token = None;
            current.last_error = Some(if snapshot.status == STATUS_FAILED {
                format!(
                    "abandoned process generation recovered; previous error: {}",
                    snapshot.last_error.as_deref().unwrap_or("unknown failure")
                )
            } else {
                "worker died unexpectedly; abandoned process generation cleaned".to_string()
            });
        } else {
            current.status = STATUS_FAILED.to_string();
            current.last_error = Some(format!(
                "abandoned process cleanup incomplete: {} survivor(s), census verified={}",
                report.survivors, report.verified
            ));
        }
        if let Err(err) = state::write_state(&current) {
            eprintln!("record abandoned generation {}: {err:#}", snapshot.id);
        }
    }
    recovered
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

    if cols < 44 || rows < 12 {
        app.rows.clear();
        app.update_click = None;
        put(
            &mut frame,
            0,
            styled_line(|buffer| {
                let _ = queue!(
                    buffer,
                    SetForegroundColor(C_ACCENT),
                    Print(fit_cols("Codex Rail", cols as usize))
                );
            }),
        );
        put(
            &mut frame,
            rows.saturating_div(2),
            styled_line(|buffer| {
                let _ = queue!(
                    buffer,
                    SetForegroundColor(C_NEEDS),
                    Print(fit_cols(
                        "terminal too small — resize to at least 44x12",
                        cols as usize
                    ))
                );
            }),
        );
    } else {
        draw_header(&mut frame, cols, app);
        draw_sessions(&mut frame, app, cols, rows);
        draw_input(&mut frame, app, cols, rows);
        draw_hint(&mut frame, app, cols, rows);
    }

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
        app.hovered_row = None;
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
                        "  No sessions yet — type your first message, then Enter (Ctrl+N for blank).",
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
                let hovered = app.hovered_row == Some(y);
                let s = &app.sessions[*index];
                let bucket = bucket_of(&app.activity, s);
                let preview = app
                    .preview
                    .get(&s.id)
                    .cloned()
                    .or_else(|| {
                        (s.status == STATUS_FAILED)
                            .then(|| s.last_error.clone())
                            .flatten()
                    })
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
                put(
                    frame,
                    y,
                    session_row_line(s, bucket, &preview, selected, hovered, cw, title_w),
                );
                app.rows.push((y, *index));
            }
        }
    }
    if app
        .hovered_row
        .is_some_and(|hovered| !app.rows.iter().any(|(row, _)| *row == hovered))
    {
        app.hovered_row = None;
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
// the full content width. A hovered row gets a quieter background and thin bar,
// but is NOT selected; moving the pointer cannot scroll, attach, or steal the
// keyboard cursor. The middle is codex's latest line (Claude Code's agents panel
// does the same); a session with no codex message yet (new or stopped) falls
// back to its path so rows stay distinguishable. The segments sum to exactly
// `cw`, so age lands at the right edge. Display-column widths keep CJK aligned.
fn session_row_line(
    session: &SessionState,
    bucket: Bucket,
    preview: &str,
    selected: bool,
    hovered: bool,
    cw: usize,
    title_w: usize,
) -> String {
    let mut age: String = format_age(last_activity_secs(session))
        .chars()
        .take(4)
        .collect();
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
    let title_color = if selected || hovered {
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
        } else if hovered {
            let _ = queue!(b, SetBackgroundColor(C_HOVER_BG));
        }
        if selected {
            let _ = queue!(b, SetForegroundColor(C_ACCENT), Print("▌"), Print(" "));
        } else if hovered {
            let _ = queue!(b, SetForegroundColor(C_ACCENT_DIM), Print("▏"), Print(" "));
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
            SetForegroundColor(if selected || hovered { C_DIM } else { C_FAINT }),
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
    if matches!(app.mode, Mode::New)
        && app.input.starts_with('/')
        && !slash_matches(&app.input).is_empty()
    {
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
    let slashing = matches!(app.mode, Mode::New)
        && app.input.starts_with('/')
        && !slash_matches(&app.input).is_empty();
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
                let safe_input = composer_display_text(&app.input);
                let visible = if display_width(&safe_input) <= text_avail {
                    safe_input
                } else {
                    fit_cols_tail(&safe_input, text_avail)
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
                        "type to message codex \u{00b7} Ctrl+N blank \u{00b7} / commands",
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
                "↑↓ move · Enter attach · Ctrl+N blank · Ctrl+A autopilot · / commands · Ctrl+R rename · Ctrl+X twice stop · Esc twice quit"
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

#[cfg(test)]
mod tests {
    use super::{
        composer_display_text, days_before, display_width, fit_cols, fit_cols_tail,
        parse_import_spec, preview_line, sanitize_composer_text, sanitize_terminal_text,
        scan_lifecycle, slash_matches, Activity, ImportSpec, Lifecycle, MAX_IMPORT_DAYS,
    };
    use std::io::Write;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn import_parser_separates_days_exact_ids_and_invalid_arguments() {
        let sid = "01900000-1234-7abc-8def-0123456789ab";
        assert_eq!(parse_import_spec("/import 15d"), Ok(ImportSpec::Days(15)));
        assert_eq!(parse_import_spec(" /import 2D "), Ok(ImportSpec::Days(2)));
        assert_eq!(
            parse_import_spec(&format!("/import {sid}")),
            Ok(ImportSpec::Session(sid.to_string()))
        );
        for invalid in [
            "/import",
            "/import 0d",
            "/import 15",
            "/import 15days",
            "/import 2d extra",
            "/import not-a-codex-id",
        ] {
            assert!(parse_import_spec(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(parse_import_spec(&format!("/import {}d", MAX_IMPORT_DAYS + 1)).is_err());
    }

    #[test]
    fn slash_matching_keeps_import_arguments_rail_side() {
        assert_eq!(slash_matches("/import 15d").len(), 1);
        assert_eq!(slash_matches("/im")[0].name, "/import");
        assert!(slash_matches("/important").is_empty());
        assert!(slash_matches("/review").is_empty());
    }

    #[test]
    fn import_day_cutoff_is_checked_and_exact() {
        let now = UNIX_EPOCH + Duration::from_secs(20 * 24 * 60 * 60);
        assert_eq!(
            days_before(now, 7),
            Some(UNIX_EPOCH + Duration::from_secs(13 * 24 * 60 * 60))
        );
        assert_eq!(days_before(UNIX_EPOCH, u64::MAX), None);
    }

    #[test]
    fn untrusted_text_cannot_emit_terminal_or_bidi_controls() {
        let hostile = "safe\u{1b}[2J\u{7}title\u{9b}31m\u{202e}txt";
        let safe = sanitize_terminal_text(hostile);
        assert_eq!(safe, "safe[2Jtitle31mtxt");
        assert!(!fit_cols(hostile, 40).contains('\u{1b}'));
        assert!(!fit_cols_tail(hostile, 8).contains('\u{9b}'));
    }

    #[test]
    fn sanitizing_controls_keeps_display_width_consistent() {
        let hostile = "甲\u{1b}[2J乙";
        assert_eq!(display_width(hostile), display_width("甲[2J乙"));
        assert_eq!(fit_cols(hostile, 8), "甲[2J乙 ");
    }

    #[test]
    fn previews_are_single_line_and_terminal_safe() {
        assert_eq!(
            preview_line("\n \u{1b}[2J hello\t world\nnext"),
            "[2J hello world"
        );
    }

    #[test]
    fn composer_preserves_multiline_payload_but_renders_one_safe_line() {
        let input = " first\r\n第二\n\tcode\u{1b}[2J";
        assert_eq!(sanitize_composer_text(input), " first\n第二\n\tcode[2J");
        assert_eq!(composer_display_text(input), " first↵第二↵⇥code[2J");
    }

    #[test]
    fn common_unicode_clusters_have_terminal_cell_widths() {
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(display_width("❤️"), 2);
        assert_eq!(display_width("👩‍💻"), 2);
        assert_eq!(display_width("🇨🇳"), 2);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width(&fit_cols("👩‍💻x", 3)), 3);
    }

    #[test]
    fn lifecycle_indexing_is_bounded_and_aborted_turns_become_waiting() {
        let path = std::env::temp_dir().join(format!(
            "rail-lifecycle-{}-{}",
            std::process::id(),
            crate::state::now_millis()
        ));
        let complete = b"{\"payload\":{\"type\":\"task_complete\"}}\n";
        let started = b"{\"payload\":{\"type\":\"task_started\"}}\n";
        let aborted = b"{\"payload\":{\"type\":\"turn_aborted\"}}\n";
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(complete).unwrap();
        file.write_all(&vec![b'x'; 9 * 1024 * 1024]).unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(started).unwrap();
        file.flush().unwrap();

        let mut lifecycle = Lifecycle {
            path: String::new(),
            offset: 0,
            last: None,
            completed_turns: 0,
            partial_record: Vec::new(),
            discarding_oversized_record: false,
            caught_up: false,
        };
        assert_eq!(
            scan_lifecycle(&mut lifecycle, path.to_str().unwrap()),
            Activity::Active
        );
        assert!(!lifecycle.caught_up);
        for _ in 0..3 {
            scan_lifecycle(&mut lifecycle, path.to_str().unwrap());
            if lifecycle.caught_up {
                break;
            }
        }
        assert!(lifecycle.caught_up);
        assert_eq!(lifecycle.last, Some(Activity::Active));
        assert_eq!(lifecycle.completed_turns, 1);

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(aborted).unwrap();
        file.flush().unwrap();
        assert_eq!(
            scan_lifecycle(&mut lifecycle, path.to_str().unwrap()),
            Activity::Waiting
        );
        assert_eq!(lifecycle.completed_turns, 1);
        let _ = std::fs::remove_file(path);
    }
}
