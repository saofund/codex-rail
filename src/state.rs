use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const APP_DIR: &str = "codex-rail";
pub const STATUS_STARTING: &str = "starting";
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_STOPPING: &str = "stopping";
pub const STATUS_EXITED: &str = "exited";
pub const STATUS_FAILED: &str = "failed";

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub codex: String,
    pub status: String,
    pub worker_pid: Option<u32>,
    pub child_pid: Option<u32>,
    pub socket: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub exit_code: Option<i32>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub codex_session_id: Option<String>,
    #[serde(default)]
    pub codex_rollout_path: Option<String>,
    // A first message handed to codex on its very first spawn (the composer
    // input in the "type = first message" flow). Consumed once by the worker,
    // then cleared so a later resume/restart doesn't replay it. None = plain.
    #[serde(default)]
    pub initial_prompt: Option<String>,
    // Set once the user renames via Ctrl+R. Pins the title so the automatic
    // "sync from codex's first message" pass leaves their chosen name alone.
    #[serde(default)]
    pub title_pinned: bool,
    // Wall-clock seconds of the last PTY output byte seen. Coarse (updated at
    // most every couple seconds, see worker.rs) "is something happening"
    // signal that doesn't require understanding what codex is actually doing.
    #[serde(default)]
    pub last_output_at: u64,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub fn new_session_id() -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", now_millis(), process::id(), n)
}

pub fn data_dir() -> PathBuf {
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(path).join(APP_DIR);
    }
    home_dir().join(".local/share").join(APP_DIR)
}

pub fn jobs_dir() -> PathBuf {
    data_dir().join("jobs")
}

pub fn job_dir(id: &str) -> PathBuf {
    jobs_dir().join(id)
}

pub fn state_path(id: &str) -> PathBuf {
    job_dir(id).join("state.json")
}

pub fn log_path(id: &str) -> PathBuf {
    job_dir(id).join("output.log")
}

pub fn label_path(id: &str) -> PathBuf {
    job_dir(id).join("label.json")
}

pub fn runtime_dir() -> PathBuf {
    if let Some(path) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(path).join(APP_DIR);
    }
    std::env::temp_dir().join(format!("{}-{}", APP_DIR, unsafe { libc::geteuid() }))
}

pub fn socket_path(id: &str) -> PathBuf {
    runtime_dir().join(format!("{id}.sock"))
}

pub fn ensure_base_dirs() -> Result<()> {
    fs::create_dir_all(jobs_dir()).context("create jobs directory")?;
    restrict_to_owner(&jobs_dir())?;
    fs::create_dir_all(runtime_dir()).context("create runtime directory")?;
    restrict_to_owner(&runtime_dir())?;
    Ok(())
}

// Session state and worker sockets can contain full terminal transcripts
// (source, secrets shown on screen). Directories default to umask-masked
// 0755 and files to 0644, which is world-readable on shared hosts, so lock
// everything down to the owner explicitly rather than relying on umask.
pub fn restrict_to_owner(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restrict permissions on {}", path.display()))
}

pub fn restrict_file_to_owner(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict permissions on {}", path.display()))
}

pub fn read_state(id: &str) -> Result<SessionState> {
    let path = state_path(id);
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

pub fn write_state(state: &SessionState) -> Result<()> {
    let dir = job_dir(&state.id);
    fs::create_dir_all(&dir).context("create job directory")?;
    restrict_to_owner(&dir)?;
    let path = state_path(&state.id);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(state).context("serialize state")?;
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    restrict_file_to_owner(&tmp)?;
    fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

// The session's user-facing label — its title and whether the user pinned it
// with a rename — lives in its OWN file, written ONLY by the manager. The
// worker rewrites state.json every couple seconds with its runtime view; if the
// title lived there too, a manager rename would be clobbered within a blink by
// any worker still holding a stale copy (exactly the "rename does nothing" bug,
// which bit hardest with old or duplicate workers that predate the title
// fields). Splitting the label into a file the worker never touches makes a
// rename immune to that race by construction. When label.json exists it is
// authoritative over whatever title happens to sit in state.json.
#[derive(Serialize, Deserialize)]
struct Label {
    title: String,
    #[serde(default)]
    title_pinned: bool,
}

pub fn read_label(id: &str) -> Option<(String, bool)> {
    let bytes = fs::read(label_path(id)).ok()?;
    let label: Label = serde_json::from_slice(&bytes).ok()?;
    Some((label.title, label.title_pinned))
}

pub fn write_label(id: &str, title: &str, title_pinned: bool) -> Result<()> {
    let dir = job_dir(id);
    fs::create_dir_all(&dir).context("create job directory")?;
    restrict_to_owner(&dir)?;
    let path = label_path(id);
    let tmp = path.with_extension("json.tmp");
    let label = Label {
        title: title.to_string(),
        title_pinned,
    };
    let bytes = serde_json::to_vec_pretty(&label).context("serialize label")?;
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    restrict_file_to_owner(&tmp)?;
    fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

pub fn load_sessions() -> Result<Vec<SessionState>> {
    let mut sessions = Vec::new();
    let dir = jobs_dir();
    if !dir.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path().join("state.json");
        if !path.exists() {
            continue;
        }
        match fs::read(&path)
            .with_context(|| format!("read {}", path.display()))
            .and_then(|bytes| {
                serde_json::from_slice::<SessionState>(&bytes)
                    .with_context(|| format!("parse {}", path.display()))
            }) {
            Ok(mut state) => {
                reconcile_liveness(&mut state);
                // label.json (manager-owned) wins over state.json's title, so a
                // worker's periodic state writes can never revert a rename.
                if let Some((title, pinned)) = read_label(&state.id) {
                    state.title = title;
                    state.title_pinned = pinned;
                }
                sessions.push(state);
            }
            Err(err) => eprintln!("skip broken session state: {err:#}"),
        }
    }

    sessions.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.title.cmp(&b.title))
    });
    Ok(sessions)
}

// A worker can die without ever writing an "exited" status (SIGKILL, OOM,
// host reboot). Without this check the manager would show such sessions as
// "running" forever, since attach only fails (and only then) once the user
// tries it.
fn reconcile_liveness(state: &mut SessionState) {
    let watched = matches!(
        state.status.as_str(),
        STATUS_STARTING | STATUS_RUNNING | STATUS_STOPPING
    );
    if !watched {
        return;
    }
    let Some(pid) = state.worker_pid else {
        return;
    };
    if worker_alive(pid) {
        return;
    }

    state.status = STATUS_EXITED.to_string();
    state.last_error = Some("worker process not found".to_string());
    state.updated_at = now_secs();
    write_state(state).ok();
}

fn worker_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

// Where the real `codex` CLI (not codex-rail) keeps its own rollout/session
// transcripts: <codex-home>/sessions/YYYY/MM/DD/rollout-*.jsonl. Honors
// CODEX_HOME (which codex itself respects) so an isolated test setup or a
// non-default codex home is picked up; falls back to ~/.codex. This layout is
// undocumented/reverse-engineered and may change between codex versions.
pub fn codex_sessions_dir() -> PathBuf {
    codex_home_dir().join("sessions")
}

// Codex's home directory (holds sessions/, history.jsonl, config.toml, ...).
// Honors CODEX_HOME like codex itself; falls back to ~/.codex.
pub fn codex_home_dir() -> PathBuf {
    if let Some(home) = env::var_os("CODEX_HOME") {
        return PathBuf::from(home);
    }
    home_dir().join(".codex")
}

// Map each codex session_id to its FIRST user message, read from codex's
// append-only history.jsonl ({session_id, ts, text} per line, in chronological
// order so the first occurrence wins). Best-effort: any error yields an empty
// map and callers just keep the title they already have. Undocumented format.
pub fn codex_first_messages() -> std::collections::HashMap<String, String> {
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let path = codex_home_dir().join("history.jsonl");
    let Ok(content) = fs::read_to_string(&path) else {
        return map;
    };
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let (Some(sid), Some(text)) = (
            value.get("session_id").and_then(|s| s.as_str()),
            value.get("text").and_then(|t| t.as_str()),
        ) else {
            continue;
        };
        map.entry(sid.to_string())
            .or_insert_with(|| text.to_string());
    }
    map
}

// codex session ids are UUIDv7, which embed a millisecond unix timestamp in
// their first 48 bits — so the id alone tells us when codex started that
// session, no file parse needed. Returns whole seconds. None if it doesn't
// look like a v7 id. (Verified against real codex 0.142.5 ids: the 12 leading
// hex digits of `019f25a9-adfd-...` decode to the session's start time.)
pub fn session_id_start_secs(session_id: &str) -> Option<u64> {
    let hex: String = session_id.chars().filter(|c| *c != '-').take(12).collect();
    if hex.len() < 12 {
        return None;
    }
    let ms = u64::from_str_radix(&hex, 16).ok()?;
    Some(ms / 1000)
}

// (cwd, session_id) from a rollout's leading `session_meta` line. Used to
// correlate an orphaned rail session (one whose worker never captured a rollout
// path — e.g. a blank session, a slow cold start, or an old worker build) with
// the codex rollout it actually belongs to. Best-effort over the same
// undocumented rollout format as the rest of the app.
pub fn rollout_head(path: &Path) -> Option<(String, String)> {
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let value: serde_json::Value = serde_json::from_str(first.trim()).ok()?;
    let payload = value.get("payload")?;
    let cwd = payload.get("cwd").and_then(|c| c.as_str())?.to_string();
    let sid = payload
        .get("session_id")
        .or_else(|| payload.get("id"))
        .and_then(|s| s.as_str())?
        .to_string();
    Some((cwd, sid))
}

// Every rollout JSONL under codex's sessions tree.
pub fn list_rollout_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_rollouts(&codex_sessions_dir(), 0, &mut out);
    out
}

fn walk_rollouts(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rollouts(&path, depth + 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}
