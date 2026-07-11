use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
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
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const STARTING_GRACE_SECS: u64 = 15;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub codex: String,
    pub status: String,
    pub worker_pid: Option<u32>,
    pub child_pid: Option<u32>,
    // New workers hold jobs/.locks/<id>.worker.lock for their whole lifetime.
    // False means a legacy state from before that protocol existed.
    #[serde(default)]
    pub worker_lock_protocol: bool,
    // A per-worker generation token. It scopes out-of-band control files to the
    // exact lock owner, so a stale stop request cannot hit a later worker after
    // pid reuse or a session resume.
    #[serde(default)]
    pub worker_token: Option<String>,
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
    // Extra CLI args passed to codex BEFORE the prompt/resume args on every
    // spawn (e.g. a distill session sets `-C <dir> -s workspace-write` and a
    // trust override so it runs autonomously). Empty for ordinary sessions.
    #[serde(default)]
    pub codex_args: Vec<String>,
    // Set when this session is an archive-distillation run: the style version it
    // is producing. Drives its distinct list label ("[distill vN]") and its
    // "Done" status (vs. an ordinary session's "Needs input") once the style file
    // lands. None for ordinary sessions.
    #[serde(default)]
    pub distill_version: Option<u32>,
    // True for an IMPORTED codex session — one discovered in ~/.codex/sessions for
    // the manager's cwd but not started by rail. In-memory only (never serialized):
    // the row is a resumable snapshot until the user attaches, at which point it's
    // resumed (codex resume) and persisted like any other session.
    #[serde(skip)]
    pub adopted: bool,
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

pub fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid session id");
    }
    Ok(())
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

// User config dir ($XDG_CONFIG_HOME/codex-rail or ~/.config/codex-rail). Holds
// durable, user-facing artifacts kept separate from the runtime job state under
// data_dir() — currently the versioned distilled-style summaries.
pub fn config_dir() -> PathBuf {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join(APP_DIR);
    }
    home_dir().join(".config").join(APP_DIR)
}

// Working root for archive distillation: the versioned `style-vNNN.md` summaries
// live here, and `corpus/` holds the freshly-aggregated, codex-readable chunks
// (regenerated every run). Also the cwd of the launched distill codex session,
// so codex only ever reads/writes inside this dir (never ~/.codex directly).
pub fn distill_dir() -> PathBuf {
    config_dir().join("distill")
}

pub fn socket_path(id: &str) -> PathBuf {
    runtime_dir().join(format!("{id}.sock"))
}

fn worker_stop_request_path(id: &str, token: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    validate_session_id(token)?;
    Ok(job_dir(id).join(format!(".stop-{token}.request")))
}

pub fn ensure_base_dirs() -> Result<()> {
    fs::create_dir_all(jobs_dir()).context("create jobs directory")?;
    restrict_to_owner(&jobs_dir())?;
    fs::create_dir_all(runtime_dir()).context("create runtime directory")?;
    restrict_to_owner(&runtime_dir())?;
    Ok(())
}

/// Create the private distillation storage and tighten any files left behind by
/// older rail builds.  The corpus and style profiles contain excerpts from the
/// user's Codex/Claude history, so relying on the process umask (commonly 022)
/// would leave them readable by other users on a shared host.
pub fn ensure_private_distill_storage() -> Result<()> {
    let config = config_dir();
    secure_private_dir(&config)?;
    secure_private_dir(&distill_dir())
}

fn secure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("refuse symlinked private directory {}", path.display())
        }
        Ok(meta) if !meta.is_dir() => bail!("private path is not a directory: {}", path.display()),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
            let meta = fs::symlink_metadata(path)
                .with_context(|| format!("inspect {}", path.display()))?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                bail!("private path is not a real directory: {}", path.display());
            }
        }
        Err(err) => return Err(err).with_context(|| format!("inspect {}", path.display())),
    }
    restrict_to_owner(path)?;
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let child = entry.path();
        if kind.is_dir() {
            secure_private_dir(&child)?;
        } else if kind.is_file() {
            restrict_file_to_owner(&child)?;
        }
        // Never follow symlinks while migrating an existing tree: a link may
        // intentionally point outside rail's private storage.
    }
    Ok(())
}

/// Write a sensitive file with 0600 from the instant it is created. Existing
/// files are chmodded before truncation by `ensure_private_distill_storage`, and
/// again here for callers writing an individual artifact.
pub fn write_private_file(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        restrict_to_owner(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("private output is not a regular file: {}", path.display());
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes.as_ref())
        .with_context(|| format!("write {}", path.display()))
}

pub fn restrict_private_file_to_owner(path: &Path) -> Result<()> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open private file {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("private path is not a regular file: {}", path.display());
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict permissions on {}", path.display()))
}

fn open_session_lock(id: &str, name: &str, nonblocking: bool) -> Result<Option<fs::File>> {
    validate_session_id(id)?;
    // Locks live outside the removable job directory. `flock` protects an inode,
    // not a pathname: unlinking job_dir/worker.lock while it was held would let a
    // second process create a new inode with the same name and split the lock.
    let dir = jobs_dir().join(".locks");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    restrict_to_owner(&dir)?;
    let path = dir.join(format!("{id}.{name}"));
    open_lock_path(&path, nonblocking)
}

fn open_lock_path(path: &Path, nonblocking: bool) -> Result<Option<fs::File>> {
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    restrict_file_to_owner(&path)?;
    let flags = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
            return Ok(Some(file));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted && !nonblocking {
            continue;
        }
        let raw = err.raw_os_error();
        if nonblocking && (raw == Some(libc::EWOULDBLOCK) || raw == Some(libc::EAGAIN)) {
            return Ok(None);
        }
        return Err(err).with_context(|| format!("lock {}", path.display()));
    }
}

/// Serialize manager-side bootstrap/removal for one session. The returned file
/// must stay alive for the whole critical section.
pub fn acquire_session_init_lock(id: &str) -> Result<fs::File> {
    open_session_lock(id, "init.lock", false)?.context("blocking lock returned no guard")
}

pub fn try_acquire_session_init_lock(id: &str) -> Result<Option<fs::File>> {
    open_session_lock(id, "init.lock", true)
}

/// Try to become the sole worker for a session. `Ok(None)` means another worker
/// owns the lifetime lock and must be left completely untouched.
pub fn try_acquire_worker_lock(id: &str) -> Result<Option<fs::File>> {
    open_session_lock(id, "worker.lock", true)
}

/// Ask a lock-aware worker to stop without relying on its Unix socket or pid.
/// This is the portable fallback when a runtime directory has been cleared:
/// the lifetime lock proves that this session still has a worker, while the
/// generation-scoped marker prevents a late request from reaching a successor.
pub fn request_worker_stop(id: &str) -> Result<()> {
    validate_session_id(id)?;
    let _init_lock = acquire_session_init_lock(id)?;
    let current = read_state(id)?;
    if !current.worker_lock_protocol {
        bail!("worker does not support out-of-band stop requests");
    }
    let token = current
        .worker_token
        .as_deref()
        .context("worker state has no generation token")?;
    if try_acquire_worker_lock(id)?.is_some() {
        bail!("session no longer has a live worker");
    }
    write_private_file(&worker_stop_request_path(id, token)?, b"stop\n")
}

/// Consume only the marker for this exact worker generation. A marker left by
/// a crashed predecessor has a different filename and is therefore inert.
pub fn take_worker_stop_request(state: &SessionState) -> Result<bool> {
    if !state.worker_lock_protocol {
        return Ok(false);
    }
    let Some(token) = state.worker_token.as_deref() else {
        return Ok(false);
    };
    let path = worker_stop_request_path(&state.id, token)?;
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("inspect {}", path.display())),
    };
    if !meta.is_file() {
        bail!("worker stop request is not a regular file: {}", path.display());
    }
    fs::remove_file(&path).with_context(|| format!("consume {}", path.display()))?;
    Ok(true)
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
    validate_session_id(id)?;
    let path = state_path(id);
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

pub fn read_state_optional(id: &str) -> Result<Option<SessionState>> {
    validate_session_id(id)?;
    let path = state_path(id);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}", path.display()))
        .map(Some)
}

pub fn write_state(state: &SessionState) -> Result<()> {
    validate_session_id(&state.id)?;
    let dir = job_dir(&state.id);
    fs::create_dir_all(&dir).context("create job directory")?;
    restrict_to_owner(&dir)?;
    let path = state_path(&state.id);
    let tmp = unique_tmp_path(&dir, "state.json");
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
    validate_session_id(id)?;
    let dir = job_dir(id);
    fs::create_dir_all(&dir).context("create job directory")?;
    restrict_to_owner(&dir)?;
    let path = label_path(id);
    let tmp = unique_tmp_path(&dir, "label.json");
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

fn unique_tmp_path(dir: &Path, stem: &str) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{stem}.tmp-{}-{n}", process::id()))
}

pub fn load_sessions() -> Result<Vec<SessionState>> {
    let mut sessions = Vec::new();
    let dir = jobs_dir();
    if !dir.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let dir_id = entry.file_name().to_string_lossy().to_string();
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
                if validate_session_id(&state.id).is_err() || state.id != dir_id {
                    eprintln!("skip session state whose id does not match its directory: {dir_id}");
                    continue;
                }
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
    if state.worker_pid.is_none() {
        if state.status == STATUS_STARTING
            && now_secs().saturating_sub(state.updated_at) > STARTING_GRACE_SECS
        {
            state.status = STATUS_EXITED.to_string();
            state.last_error = Some("worker never claimed this session".to_string());
        }
        return;
    }
    if session_worker_is_running(state) {
        return;
    }

    state.status = STATUS_EXITED.to_string();
    state.last_error = Some("worker process not found or no longer owns this session".to_string());
    // This is derived manager state only. Runtime state.json is worker-owned;
    // persisting this stale snapshot could clobber a newly-started worker's pid.
}

pub fn checked_pid(pid: u32) -> Option<libc::pid_t> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    (pid > 1).then_some(pid)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerIdentity {
    Match,
    Gone,
    Mismatch,
    Unknown,
}

fn worker_identity(pid: u32, id: &str) -> WorkerIdentity {
    let Some(raw_pid) = checked_pid(pid) else {
        return WorkerIdentity::Mismatch;
    };
    // kill(pid, 0) also succeeds for a zombie — a process that has already
    // exited but hasn't been reaped by its parent. On systems whose init
    // doesn't reap orphans (e.g. a bare container PID 1), a crashed worker
    // lingers as <defunct> indefinitely; treating it as alive would pin its
    // session to "running" forever and make it unstoppable (its socket is
    // gone, so STOP can't connect). So reject processes in the zombie state.
    if unsafe { libc::kill(raw_pid, 0) } != 0 {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => WorkerIdentity::Gone,
            _ => WorkerIdentity::Unknown,
        };
    }
    if proc_is_zombie(pid) {
        return WorkerIdentity::Gone;
    }
    if !Path::new("/proc").is_dir() {
        return WorkerIdentity::Unknown;
    }
    let cmdline = match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(cmdline) => cmdline,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return WorkerIdentity::Gone,
        // macOS and restricted /proc: keep the row live, but never use this
        // unverified identity for the PID-kill fallback.
        Err(_) => return WorkerIdentity::Unknown,
    };
    let parts: Vec<&[u8]> = cmdline
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    for pair in parts.windows(2) {
        if pair[0] == b"--worker" && pair[1] == id.as_bytes() {
            return match proc_data_dir(pid) {
                Some(dir) if dir == data_dir() => WorkerIdentity::Match,
                Some(_) => WorkerIdentity::Mismatch,
                None => WorkerIdentity::Unknown,
            };
        }
    }
    WorkerIdentity::Mismatch
}

fn proc_data_dir(pid: u32) -> Option<PathBuf> {
    let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
    let mut xdg: Option<String> = None;
    let mut home: Option<String> = None;
    for kv in environ.split(|b| *b == 0) {
        if let Some(rest) = kv.strip_prefix(b"XDG_DATA_HOME=") {
            xdg = Some(String::from_utf8_lossy(rest).into_owned());
        } else if let Some(rest) = kv.strip_prefix(b"HOME=") {
            home = Some(String::from_utf8_lossy(rest).into_owned());
        }
    }
    match xdg.filter(|x| !x.is_empty()) {
        Some(x) => Some(PathBuf::from(x).join(APP_DIR)),
        None => home
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".local/share").join(APP_DIR)),
    }
}

fn proc_is_zombie(pid: u32) -> bool {
    // /proc/<pid>/stat is "PID (comm) STATE ...". comm can contain spaces and
    // parens, so scan past the last ")"; the next non-space char is the state.
    // No /proc (non-Linux) → fall back to the kill() result (not a zombie).
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    match stat.rfind(") ") {
        Some(idx) => stat[idx + 2..].trim_start().starts_with('Z'),
        None => false,
    }
}

/// Cross-platform liveness: current workers prove life by holding worker.lock.
/// During migration, an older worker can instead prove life with a matching
/// Linux cmdline/data-dir identity or a connectable canonical socket.
pub fn session_worker_is_running(state: &SessionState) -> bool {
    if state.status == STATUS_STARTING
        && state.worker_pid.is_none()
        && now_secs().saturating_sub(state.updated_at) <= STARTING_GRACE_SECS
    {
        return true;
    }
    let _init_lock = match try_acquire_session_init_lock(&state.id) {
        Ok(Some(lock)) => lock,
        Ok(None) | Err(_) => return true,
    };
    session_worker_is_running_under_init_lock(state)
}

/// Same liveness check when the caller already owns init.lock. Acquiring the
/// same flock through a second open fd would conflict with our own guard.
pub fn session_worker_is_running_under_init_lock(state: &SessionState) -> bool {
    let _worker_lock = match try_acquire_worker_lock(&state.id) {
        Ok(Some(lock)) => lock,
        Ok(None) | Err(_) => return true,
    };
    legacy_worker_is_running(state)
}

fn legacy_worker_is_running(state: &SessionState) -> bool {
    // The caller already acquired worker.lock. For a state written by the new
    // protocol that alone proves its worker is gone, even on macOS/no-/proc where
    // a recycled pid would otherwise look "unknown but alive" forever.
    if state.worker_lock_protocol {
        // A connectable socket is still stronger live evidence if the underlying
        // filesystem's flock service is broken or an old/new binary overlaps.
        return UnixStream::connect(&state.socket).is_ok();
    }
    let identity = state
        .worker_pid
        .map(|pid| worker_identity(pid, &state.id));
    identity == Some(WorkerIdentity::Match)
        || (identity == Some(WorkerIdentity::Unknown)
            && matches!(
                state.status.as_str(),
                STATUS_STARTING | STATUS_RUNNING | STATUS_STOPPING
            ))
        || UnixStream::connect(&state.socket).is_ok()
}

/// Strong identity check used before any PID-based signal. Unknown is false:
/// being unable to prove ownership is a reason not to kill.
pub fn worker_matches_session(pid: Option<u32>, id: &str) -> bool {
    pid.map(|pid| worker_identity(pid, id) == WorkerIdentity::Match)
        .unwrap_or(false)
}

/// Delete a stopped session's on-disk footprint so it leaves the manager list.
/// The caller must ensure the worker is not running — removing a live
/// session's dir would pull the ground out from under its worker.
pub fn remove_session(id: &str) -> Result<()> {
    validate_session_id(id)?;
    let _init_lock = acquire_session_init_lock(id)?;
    let Some(_worker_lock) = try_acquire_worker_lock(id)? else {
        bail!("session still has a live worker");
    };
    if let Some(current) = read_state_optional(id)? {
        if legacy_worker_is_running(&current) {
            bail!("session state still belongs to a live worker");
        }
    }
    // Best-effort socket cleanup; a cleanly-exited worker already removed it.
    fs::remove_file(socket_path(id)).ok();
    let dir = job_dir(id);
    // Removing state.json is what actually delists the session — load_sessions
    // skips any dir without it — so that step is the one that must succeed.
    let state = dir.join("state.json");
    if state.exists() {
        fs::remove_file(&state).with_context(|| format!("remove {}", state.display()))?;
    }
    // The rest is best-effort. On a network filesystem, an output.log a
    // lingering process still holds open is silly-renamed to .nfsXXXX rather
    // than deleted, so remove_dir_all can't empty the dir; the session is
    // already gone from the list, so that must not surface as a failure.
    fs::remove_file(label_path(id)).ok();
    fs::remove_file(log_path(id)).ok();
    fs::remove_dir_all(&dir).ok();
    Ok(())
}

fn adopt_dismiss_path() -> PathBuf {
    data_dir().join(".adopt_dismissed")
}

// Codex session ids the user has "removed" from an imported (adopted) row. They
// have no on-disk footprint to delete, so we record the id here and skip it on
// the next rescan — the codex transcript itself is never touched.
pub fn adopt_dismissed() -> std::collections::HashSet<String> {
    fs::read_to_string(adopt_dismiss_path())
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

pub fn dismiss_adopted(id: &str) -> Result<()> {
    ensure_base_dirs()?;
    let mut cur = adopt_dismissed();
    if cur.insert(id.to_string()) {
        let mut body: Vec<String> = cur.into_iter().collect();
        body.sort();
        fs::write(adopt_dismiss_path(), body.join("\n") + "\n")
            .context("write adopt dismiss list")?;
    }
    Ok(())
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

// Claude Code stores its session transcripts as one .jsonl per session, under
// per-project directories: ~/.claude/projects/<project-slug>/<uuid>.jsonl.
// Distillation reads these as a second corpus source (alongside codex rollouts)
// so it can learn the user's reasoning across both tools.
pub fn claude_projects_dir() -> PathBuf {
    home_dir().join(".claude").join("projects")
}

// Codex's home directory (holds sessions/, history.jsonl, config.toml, ...).
// Honors CODEX_HOME like codex itself; falls back to ~/.codex.
pub fn codex_home_dir() -> PathBuf {
    if let Some(home) = env::var_os("CODEX_HOME") {
        return PathBuf::from(home);
    }
    home_dir().join(".codex")
}

// Map each codex session_id to its first *genuine* user message, read from
// codex's append-only history.jsonl ({session_id, ts, text} per line, in
// chronological order so the first occurrence wins). Synthetic first turns —
// slash-command echoes (<command-name>…) and codex's injected
// <environment_context>/<user_instructions> blocks — are skipped so the derived
// title is the user's real first line, not a marker. Best-effort: any error
// yields an empty map and callers just keep the title they already have.
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
        if is_synthetic_marker(text) {
            continue; // a slash-command / injected-context turn — not a title
        }
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

// codex/system messages that aren't genuine prose, so a row's title/preview skips
// past them to the real content: a whole bracketed all-caps marker
// ("<EXTERNAL SESSION IMPORTED>"), a message opening with a system `<tag>` (slash
// command echoes like <command-name>, and codex's injected
// <environment_context>/<user_instructions>/<task-notification>/reminder blocks),
// or codex's `[external_agent_*]` tool-bridge lines. Shared by the manager's
// preview (last agent message) and title (first user message) derivation.
pub fn is_synthetic_marker(s: &str) -> bool {
    let t = s.trim();
    // A whole `<...>` token in all-caps (no lowercase) — e.g. codex's
    // "<EXTERNAL SESSION IMPORTED>" — is a status marker, not prose.
    if let Some(inner) = t.strip_prefix('<').and_then(|x| x.strip_suffix('>')) {
        if !inner.is_empty() && !inner.chars().any(|c| c.is_lowercase()) {
            return true;
        }
    }
    const TAGS: [&str; 12] = [
        "<command-name",
        "<command-message",
        "<command-args",
        "<local-command",
        "<task-notification",
        "<bash-input",
        "<bash-stdout",
        "<system-reminder",
        "<environment_context",
        "<subagent_notification",
        "<user_instructions",
        "[external_agent", // codex tool bridge: [external_agent_tool_call/result]
    ];
    TAGS.iter().any(|tag| t.starts_with(tag))
}

// The first genuine user message in a rollout, to title an imported session when
// codex's history.jsonl doesn't have it (an old session, or history was cleared).
// Reads only the top of the file (the first turn is near the start). Best-effort.
// Skips codex's injected context turns (<environment_context>, <user_instructions>)
// and slash-command echoes so the title is the user's real first line, not a marker.
pub fn rollout_first_user_message(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(300).map_while(Result::ok) {
        if !line.contains("user_message") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
            continue;
        }
        let p = v.get("payload");
        if p.and_then(|p| p.get("type")).and_then(|t| t.as_str()) != Some("user_message") {
            continue;
        }
        if let Some(msg) = p.and_then(|p| p.get("message")).and_then(|m| m.as_str()) {
            if is_synthetic_marker(msg) {
                continue; // codex-injected context / slash echo — keep looking
            }
            return Some(msg.to_string());
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::{
        checked_pid, is_synthetic_marker, open_lock_path, secure_private_dir,
        validate_session_id, write_private_file,
    };
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn synthetic_markers_are_skipped() {
        // codex/slash/system markers → skipped in titles and previews
        assert!(is_synthetic_marker("<EXTERNAL SESSION IMPORTED>"));
        assert!(is_synthetic_marker("<command-name>/effort</command-name>"));
        assert!(is_synthetic_marker("<environment_context>\ncwd: /x</environment_context>"));
        assert!(is_synthetic_marker("<user_instructions>be nice</user_instructions>"));
        assert!(is_synthetic_marker("<task-notification>done</task-notification>"));
        assert!(is_synthetic_marker("  <system-reminder>hi</system-reminder>"));
        assert!(is_synthetic_marker("[external_agent_tool_result]"));
        assert!(is_synthetic_marker("[external_agent_tool_result: error]"));
        assert!(is_synthetic_marker("[external_agent_tool_call: AskUserQuestion]"));
    }

    #[test]
    fn real_prose_is_kept() {
        // genuine user/assistant text must NOT be treated as a marker
        assert!(!is_synthetic_marker("看下idependent idea 16_commute_wm"));
        assert!(!is_synthetic_marker("Let's start by reading the file."));
        assert!(!is_synthetic_marker("<html> is a lowercase tag, real prose"));
        assert!(!is_synthetic_marker("use the [brackets] like this in a sentence"));
        assert!(!is_synthetic_marker(""));
    }

    #[test]
    fn private_tree_migration_locks_down_existing_and_new_artifacts() {
        let root = std::env::temp_dir().join(format!(
            "rail-private-tree-{}-{}",
            std::process::id(),
            super::now_millis()
        ));
        let nested = root.join("corpus");
        fs::create_dir_all(&nested).unwrap();
        let old = nested.join("corpus-01.md");
        fs::write(&old, "private history").unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o644)).unwrap();

        secure_private_dir(&root).unwrap();
        assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::metadata(&old).unwrap().permissions().mode() & 0o777, 0o600);

        let new = nested.join("style-v001.md");
        write_private_file(&new, "profile").unwrap();
        assert_eq!(fs::metadata(&new).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_lock_is_exclusive_and_released_with_its_guard() {
        let root = std::env::temp_dir().join(format!(
            "rail-worker-lock-{}-{}",
            std::process::id(),
            super::now_millis()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("worker.lock");
        let first = open_lock_path(&path, true).unwrap().unwrap();
        assert!(open_lock_path(&path, true).unwrap().is_none());
        drop(first);
        assert!(open_lock_path(&path, true).unwrap().is_some());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn session_ids_and_pids_fail_closed() {
        for id in ["abc-123", "seed_0", "A1"] {
            assert!(validate_session_id(id).is_ok());
        }
        for id in ["", "../escape", "a/b", ".", "has space"] {
            assert!(validate_session_id(id).is_err(), "accepted {id:?}");
        }
        assert!(checked_pid(0).is_none());
        assert!(checked_pid(1).is_none());
        assert!(checked_pid(u32::MAX).is_none());
    }

    #[test]
    fn private_storage_never_follows_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "rail-private-symlink-{}-{}",
            std::process::id(),
            super::now_millis()
        ));
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let target = outside.join("target.txt");
        fs::write(&target, "keep me").unwrap();

        let linked_dir = root.join("distill-link");
        symlink(&outside, &linked_dir).unwrap();
        assert!(secure_private_dir(&linked_dir).is_err());

        let output_link = root.join("prompt-link");
        symlink(&target, &output_link).unwrap();
        assert!(write_private_file(&output_link, "overwrite").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
        let _ = fs::remove_dir_all(&root);
    }
}
