use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
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
