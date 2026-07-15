//! Archive distillation.
//!
//! Distills the user's personal working style AND problem-solving logic from
//! their real past conversations — both codex rollouts (`~/.codex/sessions`)
//! and Claude Code sessions (`~/.claude/projects`). For each session we keep the
//! user's OWN turns *in context*: a compressed `[assistant]` lead-in of what the
//! assistant had just said or done (and which tools it ran) precedes each user
//! turn, so the distiller can see not only HOW the user writes but WHY they steer
//! the way they do — what they approve, what they reject, how they open a task
//! and drive it to done.
//!
//! The raw archives are huge (hundreds of MB of codex, GBs of Claude), most of it
//! re-injected context, tool output and reasoning. We scan the most-recent files,
//! rank sessions by richness (number of user turns = number of decision points),
//! and pack the richest into a bounded, **fully-readable** corpus split into
//! numbered chunks. A per-chunk marker echoed in a machine-readable footer is a
//! strong completion contract (it rejects stale/partial outputs), not a claim
//! that arbitrary model behavior can cryptographically prove every byte read.

use crate::state;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::{cmp::Reverse, fs};

// ---- tunables (all env-overridable so tests can force small shapes) ----------

// Chars kept per user turn and assistant lead-in, plus the per-chunk and total
// corpus byte budgets.
const MSG_CAP_DEFAULT: usize = 700;
const LEAD_CAP_DEFAULT: usize = 220;
const CHUNK_BYTES_DEFAULT: usize = 200_000;
const BUDGET_BYTES_DEFAULT: usize = 4_000_000;
// Tiered sampling: rather than read the whole multi-GB archive, we stat every
// file (cheap) and then READ only a bounded candidate set — the BIGGEST files
// (most content = a richness proxy) plus the NEWEST (current working style),
// capped at SCAN_FILES per source. The richness ranking then refines within
// those candidates. This keeps the scan lean and bounded no matter how large the
// archive grows. All caps are env-overridable; HUGE_FILE_BYTES skips a single
// pathological transcript before reading it.
const SCAN_FILES_DEFAULT: usize = 1000;
const SCAN_BYTES_DEFAULT: usize = 500 * 1024 * 1024;
// Skip a single transcript bigger than this before reading it: past ~12 MB a
// session is almost all tool output, not user turns, so it's expensive to read
// and turn-poor — skipping it lets many more turn-dense sessions fit the budget.
const HUGE_FILE_BYTES: u64 = 12 * 1024 * 1024;
// Maximum turns kept per session (head + tail if longer).
const TURN_CAP: usize = 64;
// A line this long is only ever a giant tool_call/tool_result blob — a human turn
// or an assistant lead-in never approaches it. Skipping such lines (see
// skippable_blob) before JSON-parsing is what keeps prepare() fast on multi-MB
// transcripts, WITHOUT dropping the user's long pasted turns (those lack the tool
// markers, so they're kept and capped later).
const MAX_LINE_BYTES: usize = 24_000;
static TRUST_THREAD_LOCK: Mutex<()> = Mutex::new(());
static TRUST_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static DISTILL_RUN_COUNTER: AtomicU64 = AtomicU64::new(0);
pub const DISTILL_LOCK_FD_ENV: &str = "CODEX_RAIL_DISTILL_LOCK_FD";

struct TrustLock {
    _thread: MutexGuard<'static, ()>,
    _file: fs::File,
}

struct TrustTemp(PathBuf);

impl Drop for TrustTemp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn msg_cap() -> usize {
    env_usize("CODEX_RAIL_DISTILL_MSG_CAP", MSG_CAP_DEFAULT)
}
fn lead_cap() -> usize {
    env_usize("CODEX_RAIL_DISTILL_LEAD_CAP", LEAD_CAP_DEFAULT)
}
fn chunk_bytes() -> usize {
    env_usize("CODEX_RAIL_DISTILL_CHUNK_BYTES", CHUNK_BYTES_DEFAULT).max(200)
}
fn budget_bytes() -> usize {
    env_usize("CODEX_RAIL_DISTILL_BUDGET_BYTES", BUDGET_BYTES_DEFAULT).max(1000)
}
fn scan_files() -> usize {
    env_usize("CODEX_RAIL_DISTILL_SCAN_FILES", SCAN_FILES_DEFAULT).max(1)
}
fn scan_bytes() -> usize {
    env_usize("CODEX_RAIL_DISTILL_SCAN_BYTES", SCAN_BYTES_DEFAULT).max(1024 * 1024)
}
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// ---- data model --------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Src {
    Codex,
    Claude,
}
impl Src {
    fn tag(self) -> &'static str {
        match self {
            Src::Codex => "CODEX",
            Src::Claude => "CLAUDE",
        }
    }
}

enum Role {
    User,
    Assistant,
}

struct Turn {
    role: Role,
    text: String,
    tools: Vec<String>,
}

// One session's turns in chronological order, plus where/when it happened.
struct Convo {
    src: Src,
    date: String,
    project: String,
    turns: Vec<Turn>,
}
impl Convo {
    fn user_turns(&self) -> usize {
        self.turns
            .iter()
            .filter(|t| matches!(t.role, Role::User))
            .count()
    }
}

pub struct Chunk {
    pub file: String,   // e.g. "corpus-01.md" (relative to corpus/)
    pub marker: String, // the id echoed on this chunk's trailing <<CHUNK…>> line
}

struct RunCorpusGuard {
    corpus_rel: String,
    armed: bool,
}

impl RunCorpusGuard {
    fn cleanup(&mut self) -> Result<()> {
        if self.armed {
            cleanup_run_corpus(&self.corpus_rel)?;
            self.armed = false;
        }
        Ok(())
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for RunCorpusGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = cleanup_run_corpus(&self.corpus_rel);
        }
    }
}

pub struct DistillPrep {
    pub workdir: PathBuf,       // distill_dir(); the launched session's cwd
    pub version: u32,           // next style version (1-based)
    pub output_file: String,    // "style-v001.md", relative to workdir
    pub sessions: usize,        // sessions included in the corpus
    pub messages: usize,        // user turns included
    pub codex_sessions: usize,  // of `sessions`, how many are from codex
    pub claude_sessions: usize, // …and from Claude Code
    pub scanned: usize,         // transcript files actually read
    pub available: usize,       // sessions with >=1 real user turn found while scanning
    pub chunks: Vec<Chunk>,
    pub corpus_rel: String,
    pub run_lock: fs::File,
    corpus_guard: RunCorpusGuard,
}

impl DistillPrep {
    /// Transfer cleanup ownership to the durable session state/worker after a
    /// successful launch (or deliberately keep a headless prepared corpus).
    pub fn commit_corpus(&mut self) {
        self.corpus_guard.commit();
    }

    pub fn cleanup_corpus(&mut self) -> Result<()> {
        self.corpus_guard.cleanup()
    }
}

/// Aggregate the archives into a unique `distill_dir()/runs/run-*/corpus/` and
/// return a plan the launcher turns into a Codex session.
pub fn prepare() -> Result<DistillPrep> {
    // This tree contains verbatim excerpts from private Codex/Claude history.
    // Migrate artifacts created by older builds before reading or rewriting it.
    state::ensure_private_distill_storage()?;
    let run_lock = acquire_run_lock()?;
    let workdir = state::distill_dir();
    let run_counter = DISTILL_RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let run_id = format!(
        "run-{:x}-{:x}-{run_counter:x}",
        state::now_millis(),
        std::process::id()
    );
    // Every run gets immutable input paths. The lifetime lock still serializes
    // version allocation and active jobs, while unique directories make a
    // crashed/old session incapable of reading a later run's rewritten corpus.
    let corpus_rel = format!("runs/{run_id}/corpus");
    let runs_dir = workdir.join("runs");
    let run_dir = runs_dir.join(&run_id);
    let corpus_dir = run_dir.join("corpus");
    // create_dir_all would expose intermediate `runs/` and `run-*` directories
    // through the process umask (commonly 0755). Validate and chmod each level
    // before creating the next because every one contains private history.
    state::ensure_private_directory(&runs_dir)?;
    state::ensure_private_directory(&run_dir)?;
    let mut corpus_guard = RunCorpusGuard {
        corpus_rel: corpus_rel.clone(),
        armed: true,
    };
    state::ensure_private_directory(&corpus_dir)?;

    let cap = msg_cap();
    let lead = lead_cap();
    let (mut convos, scanned) = scan_all();
    let available = convos.len();

    // Rank by richness (more user turns = more decision points), recent as
    // tiebreak. This surfaces the substantive back-and-forth sessions where the
    // user's reasoning shows, over one-line throwaways.
    convos.sort_by(|a, b| {
        b.user_turns()
            .cmp(&a.user_turns())
            .then_with(|| b.date.cmp(&a.date))
    });

    // Fill the corpus balanced across sources BY BYTES: take richest-first within
    // each source, always extending whichever source has contributed fewer bytes
    // so far. Claude has ~25x more sessions than codex, so a single global ranking
    // would let it crowd codex out entirely; balancing keeps both tools well
    // represented (and spills to the other source once one is exhausted).
    let budget = budget_bytes();
    let codex: Vec<&Convo> = convos.iter().filter(|c| c.src == Src::Codex).collect();
    let claude: Vec<&Convo> = convos.iter().filter(|c| c.src == Src::Claude).collect();
    let (mut ci, mut li) = (0usize, 0usize);
    let (mut cx_bytes, mut cl_bytes) = (0usize, 0usize);
    let mut body_blocks: Vec<String> = Vec::new();
    let mut user_msgs = 0usize;
    let (mut inc_codex, mut inc_claude) = (0usize, 0usize);
    loop {
        if cx_bytes + cl_bytes >= budget {
            break;
        }
        // Prefer the source with fewer bytes so far; fall back when one is spent.
        let take_codex = match (ci < codex.len(), li < claude.len()) {
            (true, true) => cx_bytes <= cl_bytes,
            (true, false) => true,
            (false, true) => false,
            (false, false) => break,
        };
        let idx = body_blocks.len() + 1;
        let c = if take_codex {
            let c = codex[ci];
            ci += 1;
            inc_codex += 1;
            c
        } else {
            let c = claude[li];
            li += 1;
            inc_claude += 1;
            c
        };
        let kept_user_turns = clip_turns(&c.turns, TURN_CAP)
            .into_iter()
            .filter(|turn| matches!(turn.role, Role::User))
            .count();
        let block = format_convo(c, idx, cap, lead);
        // Count roles in the clipped structure, never rendered line prefixes:
        // a multiline user paste may itself contain lines beginning `> `.
        user_msgs += kept_user_turns;
        if take_codex {
            cx_bytes += block.len();
        } else {
            cl_bytes += block.len();
        }
        body_blocks.push(block);
    }
    let included = body_blocks.len();
    if included == 0 || user_msgs == 0 {
        corpus_guard
            .cleanup()
            .context("clean empty private distillation corpus before returning")?;
        bail!("no genuine user history was found to distill");
    }

    // Pack blocks into chunk-sized buffers. A session may span a chunk boundary —
    // fine, codex reads every chunk in order.
    let chunk_bytes = chunk_bytes();
    let mut buffers: Vec<String> = Vec::new();
    let mut cur = String::new();
    for block in body_blocks {
        if !cur.is_empty() && cur.len() + block.len() > chunk_bytes {
            buffers.push(std::mem::take(&mut cur));
        }
        cur.push_str(&block);
    }
    if !cur.is_empty() {
        buffers.push(cur);
    }

    // Write each file with a header and a trailing marker whose id is unique to
    // this run (so a stale style file can't pass verification).
    let n = buffers.len();
    let salt = format!("{run_id}:{}", state::now_millis());
    let mut chunks = Vec::with_capacity(n);
    for (i, body) in buffers.into_iter().enumerate() {
        let k = i + 1;
        let file = format!("corpus-{k:02}.md");
        let marker = short_hash(&format!("{salt}:{k}"));
        let content = format!(
            "# corpus chunk {k}/{n} — the user's own conversations, in context (read this file fully)\n{body}\n<<CHUNK {k}/{n} id={marker}>>\n"
        );
        state::write_private_file(&corpus_dir.join(&file), content)
            .with_context(|| format!("write corpus chunk {file}"))?;
        chunks.push(Chunk { file, marker });
    }

    // Permanently reserve the version before launching Codex. A failed v001
    // must not let a later run reuse v001, because the old unique-corpus session
    // may still be resumed and publish its own output later.
    let version = reserve_version(&workdir, &run_id)?;
    Ok(DistillPrep {
        workdir,
        version,
        output_file: format!("style-v{version:03}.md"),
        sessions: included,
        messages: user_msgs,
        codex_sessions: inc_codex,
        claude_sessions: inc_claude,
        scanned,
        available,
        chunks,
        corpus_rel,
        run_lock,
        corpus_guard,
    })
}

fn run_lock_path() -> PathBuf {
    state::distill_dir().join(".active.lock")
}

/// Hold this file for the complete prepare -> Codex -> validation lifetime.
/// `flock` survives unlink/path races only while the inode remains, so the lock
/// file is permanent and private rather than removed on completion.
pub fn acquire_run_lock() -> Result<fs::File> {
    state::ensure_private_distill_storage()?;
    let path = run_lock_path();
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("open distill lock {}", path.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        bail!("distill lock is not a private owned regular file");
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        let raw = err.raw_os_error();
        if raw == Some(libc::EWOULDBLOCK) || raw == Some(libc::EAGAIN) {
            bail!("another style distillation is already preparing or running");
        }
        return Err(err).context("lock distillation run");
    }
    Ok(file)
}

/// Clear close-on-exec so the manager can hand the already-held open-file
/// description to its worker without a lock-release gap.
pub fn make_run_lock_inheritable(file: &fs::File) -> Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("read distill lock fd flags");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error()).context("make distill lock inheritable");
    }
    Ok(())
}

/// Take the lock inherited from the launching manager, or acquire it on an
/// explicit resume. The fd is validated against the canonical lock inode before
/// ownership is accepted, then made close-on-exec again so Codex never inherits
/// the lease itself.
pub fn worker_run_lock() -> Result<fs::File> {
    let Some(raw) = std::env::var_os(DISTILL_LOCK_FD_ENV) else {
        return acquire_run_lock();
    };
    std::env::remove_var(DISTILL_LOCK_FD_ENV);
    let raw = raw
        .to_str()
        .and_then(|value| value.parse::<libc::c_int>().ok())
        .filter(|fd| *fd >= 3)
        .context("invalid inherited distill lock fd")?;
    // SAFETY: the manager deliberately inherited this unique descriptor and
    // ownership is transferred exactly once at worker startup.
    let file = unsafe { fs::File::from_raw_fd(raw) };
    let fd_meta = file.metadata().context("inspect inherited distill lock")?;
    let path_meta = fs::symlink_metadata(run_lock_path()).context("inspect distill lock path")?;
    if !fd_meta.is_file()
        || path_meta.file_type().is_symlink()
        || !path_meta.is_file()
        || fd_meta.dev() != path_meta.dev()
        || fd_meta.ino() != path_meta.ino()
        || fd_meta.uid() != unsafe { libc::geteuid() }
        || fd_meta.mode() & 0o077 != 0
    {
        bail!("inherited distill lock does not match the trusted lock inode");
    }
    // flock belongs to the open-file description and survives fork/exec.  A
    // second nonblocking LOCK_EX both confirms the genuine inherited
    // description and safely acquires the lease if a manually invoked worker
    // supplied the canonical inode without a lock.  It fails closed when any
    // other description owns the lifetime lease.
    claim_inherited_run_lock(&file)?;
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error()).context("seal inherited distill lock fd");
    }
    Ok(file)
}

fn claim_inherited_run_lock(file: &fs::File) -> Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("inherited distill descriptor does not own the lifetime lock");
    }
    Ok(())
}

/// Validate the machine-readable footer required by the distillation prompt.
/// A non-empty file alone is not success: every per-run marker must appear
/// exactly once and the claimed USER-turn count must equal the prepared corpus.
pub fn validate_output_contract(
    path: &Path,
    expected_markers: &[String],
    expected_user_turns: usize,
) -> Result<()> {
    if expected_markers.is_empty() {
        bail!("distill completion contract has no chunk markers");
    }
    const MAX_STYLE_BYTES: u64 = 8 * 1024 * 1024;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open distilled style {}", path.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_STYLE_BYTES {
        bail!("distilled style is empty, oversized, or not a regular file");
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_STYLE_BYTES + 1).read_to_end(&mut bytes)?;
    let text = std::str::from_utf8(&bytes).context("distilled style is not UTF-8")?;
    let coverage_at = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == "## Coverage")
        .map(|(index, _)| index)
        .last()
        .context("distilled style is missing an exact ## Coverage section")?;
    let coverage: Vec<&str> = text.lines().skip(coverage_at + 1).map(str::trim).collect();
    let mut found = Vec::new();
    let mut turns = Vec::new();
    for line in coverage {
        if let Some(marker) = line.strip_prefix("CHUNK_ID=") {
            found.push(marker.to_string());
        } else if let Some(value) = line.strip_prefix("USER_TURNS_READ=") {
            turns.push(
                value
                    .parse::<usize>()
                    .context("invalid USER_TURNS_READ value")?,
            );
        }
    }
    let expected: HashSet<&str> = expected_markers.iter().map(String::as_str).collect();
    let actual: HashSet<&str> = found.iter().map(String::as_str).collect();
    if found.len() != expected_markers.len() || actual.len() != found.len() || actual != expected {
        bail!("Coverage chunk markers are missing, duplicated, or from another run");
    }
    if turns.as_slice() != [expected_user_turns] {
        bail!(
            "Coverage USER turn count does not match prepared corpus (expected {expected_user_turns})"
        );
    }
    state::restrict_private_file_to_owner(path)?;
    Ok(())
}

pub fn mark_output_validated(version: u32) -> Result<()> {
    let marker = state::distill_dir().join(format!("style-v{version:03}.validated"));
    state::write_private_file_atomic(&marker, b"validated-by-codex-rail\n")
}

/// Return a style only when both it and Rail's durable validation marker are
/// private owned regular files and the marker has the exact format written
/// above.  A directory entry named `.validated` is not itself a trust signal.
pub fn validated_style_file(version: u32) -> Option<PathBuf> {
    validated_style_file_at(&state::distill_dir(), version)
}

fn validated_style_file_at(root: &Path, version: u32) -> Option<PathBuf> {
    const MARKER: &[u8] = b"validated-by-codex-rail\n";
    let marker_path = root.join(format!("style-v{version:03}.validated"));
    let mut marker = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&marker_path)
        .ok()?;
    let marker_meta = marker.metadata().ok()?;
    if !marker_meta.is_file()
        || marker_meta.uid() != unsafe { libc::geteuid() }
        || marker_meta.mode() & 0o077 != 0
        || marker_meta.len() != MARKER.len() as u64
    {
        return None;
    }
    let mut marker_bytes = Vec::with_capacity(MARKER.len());
    marker.read_to_end(&mut marker_bytes).ok()?;
    if marker_bytes != MARKER {
        return None;
    }

    let style_path = root.join(format!("style-v{version:03}.md"));
    let style = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&style_path)
        .ok()?;
    let style_meta = style.metadata().ok()?;
    if !style_meta.is_file()
        || style_meta.uid() != unsafe { libc::geteuid() }
        || style_meta.mode() & 0o077 != 0
        || style_meta.len() == 0
        || style_meta.len() > 8 * 1024 * 1024
    {
        return None;
    }
    Some(style_path)
}

/// Remove only the immutable run directory shape minted by `prepare`.  Never
/// accept an absolute path, `..`, or a symlinked run root from persisted state.
pub fn cleanup_run_corpus(corpus_rel: &str) -> Result<()> {
    state::ensure_private_distill_storage()
        .context("validate private distill root before corpus cleanup")?;
    let rel = Path::new(corpus_rel);
    let parts: Vec<_> = rel.components().collect();
    let valid = matches!(
        parts.as_slice(),
        [
            std::path::Component::Normal(runs),
            std::path::Component::Normal(run),
            std::path::Component::Normal(corpus)
        ] if *runs == std::ffi::OsStr::new("runs")
            && run.to_string_lossy().starts_with("run-")
            && *corpus == std::ffi::OsStr::new("corpus")
    );
    if !valid {
        bail!("refuse invalid distill corpus path {corpus_rel:?}");
    }
    let runs_dir = state::distill_dir().join("runs");
    // Reject a symlinked intermediate before remove_dir_all sees it.  This is
    // both an old-layout migration guard and protection against a corrupted
    // persisted corpus path redirecting deletion outside Rail's private root.
    state::ensure_private_directory(&runs_dir)
        .context("validate private distill runs directory before cleanup")?;
    let run_dir = runs_dir.join(match &parts[1] {
        std::path::Component::Normal(run) => run,
        _ => unreachable!("validated above"),
    });
    let meta = match fs::symlink_metadata(&run_dir) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context("inspect distill run directory"),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        bail!("refuse unsafe distill run directory {}", run_dir.display());
    }
    fs::remove_dir_all(&run_dir)
        .with_context(|| format!("remove completed distill corpus {}", run_dir.display()))
}

// Scan both sources (a biggest+newest candidate sample per source) into
// contextualized conversations. Returns the convos with >=1 real user turn and
// how many files were actually read.
fn scan_all() -> (Vec<Convo>, usize) {
    let cap = scan_files();
    let mut convos = Vec::new();
    let mut scanned = 0;
    scanned += scan_codex(cap, &mut convos);
    scanned += scan_claude(cap, &mut convos);
    convos.retain(|c| c.user_turns() > 0);
    (convos, scanned)
}

// From the full file list, pick up to `cap` candidates to actually READ: the
// NEWEST (~1/4 of the cap) plus the BIGGEST (the rest). Stat-only, so it never
// reads a file it won't use — the cheap tier that bounds a huge archive to a
// representative sample before the (expensive) parse. Dedups the overlap.
fn pick_candidates(files: Vec<PathBuf>, cap: usize) -> Vec<PathBuf> {
    if files.len() <= cap {
        return files;
    }
    let metas: Vec<(PathBuf, u64, std::time::SystemTime)> = files
        .into_iter()
        .map(|p| {
            let (sz, mt) = fs::metadata(&p)
                .map(|m| (m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH)))
                .unwrap_or((0, std::time::UNIX_EPOCH));
            (p, sz, mt)
        })
        .collect();
    let newest_n = cap / 4;
    let mut seen = std::collections::HashSet::new();
    let mut chosen: Vec<PathBuf> = Vec::with_capacity(cap);
    let mut by_time: Vec<&(PathBuf, u64, std::time::SystemTime)> = metas.iter().collect();
    by_time.sort_by_key(|(_, _, m)| Reverse(*m));
    for (p, _, _) in by_time.into_iter().take(newest_n) {
        if seen.insert(p.clone()) {
            chosen.push(p.clone());
        }
    }
    let mut by_size: Vec<&(PathBuf, u64, std::time::SystemTime)> = metas.iter().collect();
    by_size.sort_by_key(|(_, s, _)| Reverse(*s));
    for (p, _, _) in by_size {
        if chosen.len() >= cap {
            break;
        }
        if seen.insert(p.clone()) {
            chosen.push(p.clone());
        }
    }
    chosen
}

// codex rollouts: user_message (user), agent_message (assistant lead-in),
// function_call (tool names attached to the surrounding assistant turn).
fn scan_codex(cap: usize, out: &mut Vec<Convo>) -> usize {
    let mut files = Vec::new();
    walk_files(&state::codex_sessions_dir(), 0, &mut files, &|n| {
        n.starts_with("rollout-") && n.ends_with(".jsonl")
    });
    let files = pick_candidates(files, cap); // biggest + newest, capped

    let mut scanned = 0;
    let mut bytes = 0usize;
    let byte_cap = scan_bytes();
    for path in files {
        if bytes >= byte_cap {
            break;
        }
        if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > HUGE_FILE_BYTES {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        scanned += 1;
        bytes += content.len();
        let date = date_from_path(&path);
        let mut project = String::new();
        let mut turns: Vec<Turn> = Vec::new();
        let mut pending_tools: Vec<String> = Vec::new();
        for line in content.lines() {
            if skippable_blob(line) {
                continue; // a giant tool blob — skip before JSON-parsing
            }
            // Cheap pre-filter: only these payload kinds carry a turn or metadata.
            if !(line.contains("user_message")
                || line.contains("agent_message")
                || line.contains("function_call")
                || line.contains("session_meta"))
            {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let p = v.get("payload");
            let pty = p
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if ty == "session_meta" {
                if project.is_empty() {
                    if let Some(cwd) = p
                        .and_then(|p| p.get("cwd"))
                        .or_else(|| v.get("cwd"))
                        .and_then(|c| c.as_str())
                    {
                        project = project_base(cwd);
                    }
                }
            } else if ty == "event_msg" && pty == "user_message" {
                let text = p
                    .and_then(|p| p.get("message").or_else(|| p.get("text")))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if is_real_user(text) {
                    // Assistant acted (tools) without a visible message before this
                    // user turn — record the actions so the turn stays interpretable.
                    if !pending_tools.is_empty() {
                        turns.push(Turn {
                            role: Role::Assistant,
                            text: String::new(),
                            tools: std::mem::take(&mut pending_tools),
                        });
                    }
                    turns.push(Turn {
                        role: Role::User,
                        text: text.trim().to_string(),
                        tools: Vec::new(),
                    });
                }
            } else if ty == "event_msg" && pty == "agent_message" {
                let text = p
                    .and_then(|p| p.get("message"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                turns.push(Turn {
                    role: Role::Assistant,
                    text: text.trim().to_string(),
                    tools: std::mem::take(&mut pending_tools),
                });
            } else if ty == "response_item" && pty == "function_call" {
                if pending_tools.len() < 6 {
                    let name = p
                        .and_then(|p| p.get("name"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("tool");
                    pending_tools.push(name.to_string());
                }
            }
        }
        if !turns.is_empty() {
            out.push(Convo {
                src: Src::Codex,
                date,
                project,
                turns,
            });
        }
    }
    scanned
}

// Claude Code transcripts: user (str = human; [tool_result] = injected, dropped),
// assistant ([text] lead-in + [tool_use] names).
fn scan_claude(cap: usize, out: &mut Vec<Convo>) -> usize {
    let mut files = Vec::new();
    walk_files(&state::claude_projects_dir(), 0, &mut files, &|n| {
        n.ends_with(".jsonl")
    });
    let files = pick_candidates(files, cap); // biggest + newest, capped

    let mut scanned = 0;
    let mut bytes = 0usize;
    let byte_cap = scan_bytes();
    for path in files {
        if bytes >= byte_cap {
            break;
        }
        if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > HUGE_FILE_BYTES {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        scanned += 1;
        bytes += content.len();
        let mut date = String::new();
        let mut project = String::new();
        let mut turns: Vec<Turn> = Vec::new();
        for line in content.lines() {
            if skippable_blob(line) {
                continue; // a giant tool_result/tool_use blob — skip before parsing
            }
            if !(line.contains("\"user\"") || line.contains("\"assistant\"")) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if ty != "user" && ty != "assistant" {
                continue;
            }
            if date.is_empty() {
                if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                    if ts.len() >= 10 {
                        date = ts[..10].to_string();
                    }
                }
            }
            if project.is_empty() {
                if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                    project = project_base(cwd);
                }
            }
            let (text, tools) =
                extract_claude_content(v.get("message").and_then(|m| m.get("content")));
            if ty == "user" {
                if is_real_user(&text) {
                    turns.push(Turn {
                        role: Role::User,
                        text: text.trim().to_string(),
                        tools: Vec::new(),
                    });
                }
            } else if !text.trim().is_empty() || !tools.is_empty() {
                turns.push(Turn {
                    role: Role::Assistant,
                    text: text.trim().to_string(),
                    tools,
                });
            }
        }
        if date.is_empty() {
            date = "unknown-date".to_string();
        }
        if !turns.is_empty() {
            out.push(Convo {
                src: Src::Claude,
                date,
                project,
                turns,
            });
        }
    }
    scanned
}

// Pull the human text and tool names out of a Claude `message.content`, which is
// either a bare string or a list of typed blocks. A user turn that is ONLY a
// tool_result is injected output, not the human — return empty so it's dropped.
fn extract_claude_content(c: Option<&serde_json::Value>) -> (String, Vec<String>) {
    match c {
        Some(serde_json::Value::String(s)) => (s.clone(), Vec::new()),
        Some(serde_json::Value::Array(arr)) => {
            let mut text = String::new();
            let mut tools = Vec::new();
            let mut has_tool_result = false;
            for b in arr {
                match b.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                text.push(' ');
                            }
                            text.push_str(t);
                        }
                    }
                    "tool_use" => {
                        if tools.len() < 6 {
                            if let Some(nm) = b.get("name").and_then(|t| t.as_str()) {
                                tools.push(nm.to_string());
                            }
                        }
                    }
                    "tool_result" => has_tool_result = true,
                    _ => {}
                }
            }
            if has_tool_result && text.is_empty() {
                return (String::new(), Vec::new());
            }
            (text, tools)
        }
        _ => (String::new(), Vec::new()),
    }
}

// Render one session: `> ` user turns verbatim (capped), `[assistant]` context
// lines compressed to a lead-in + tool summary. Long sessions are clipped to a
// head+tail window (noted in the header) so one giant session can't eat the budget.
fn format_convo(c: &Convo, idx: usize, cap: usize, lead: usize) -> String {
    let nuser = c.user_turns();
    let clipped = c.turns.len() > TURN_CAP;
    let kept = clip_turns(&c.turns, TURN_CAP);
    let proj = if c.project.is_empty() {
        "-"
    } else {
        &c.project
    };
    let note = if clipped {
        format!(" · (long: {} of {} turns)", kept.len(), c.turns.len())
    } else {
        String::new()
    };
    let mut s = format!(
        "\n===== {} SESSION {} · {} · {} · {} user turn(s){} =====\n",
        c.src.tag(),
        idx,
        c.date,
        proj,
        nuser,
        note
    );
    for t in kept {
        match t.role {
            Role::User => {
                let text = cap_chars(t.text.trim(), cap);
                for (line_index, line) in text.lines().enumerate() {
                    s.push_str(if line_index == 0 { "> " } else { "> | " });
                    s.push_str(line);
                    s.push('\n');
                }
            }
            Role::Assistant => {
                let mut line = String::from("[assistant] ");
                let lead_txt = one_line(&t.text);
                if !lead_txt.is_empty() {
                    line.push_str(&cap_chars(&lead_txt, lead));
                }
                if !t.tools.is_empty() {
                    line.push_str(&format!(" · did: {}", dedup_join(&t.tools)));
                }
                if line.trim() == "[assistant]" {
                    continue; // nothing to show
                }
                s.push_str(&line);
                s.push('\n');
            }
        }
    }
    s
}

// Keep the first 2/3 and last 1/3 of a long turn list; a marker turn is not
// inserted (the header already notes the clip) to keep this dependency-free.
fn clip_turns(turns: &[Turn], cap: usize) -> Vec<&Turn> {
    if turns.len() <= cap {
        return turns.iter().collect();
    }
    let head = cap * 2 / 3;
    let tail = cap - head;
    let mut v: Vec<&Turn> = turns[..head].iter().collect();
    v.extend(turns[turns.len() - tail..].iter());
    v
}

// A user_message can also carry tool/system-injected blobs that surface as
// user-role text (subagent notifications, environment context, slash-command
// caveats, the first-turn instructions). Those aren't the human's voice — drop
// them. Also drops the distiller's own prompt so a distill session can't feed on
// itself.
fn is_real_user(text: &str) -> bool {
    let s = text.trim();
    if s.is_empty() {
        return false;
    }
    const BLOBS: [&str; 14] = [
        "<subagent_notification>",
        "<system-reminder>",
        "<environment_context>",
        "<user_instructions>",
        "<INSTRUCTIONS>",
        "<local-command",
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "IMPORTANT: this context",
        "Caveat: The messages below",
        "[Request interrupted",
        "This session is being continued from a previous",
        "You are analyzing the user's OWN past",
    ];
    for b in BLOBS {
        if s.contains(b) {
            return false;
        }
    }
    // A message that is entirely one XML-ish tag block is not prose.
    if s.starts_with('<') && s.ends_with('>') {
        return false;
    }
    true
}

// A line worth skipping before the (expensive) JSON parse: only the giant
// tool_call / tool_result blobs, never a human turn (which lacks these markers
// and is kept, then capped). Keeps prepare() fast without dropping real turns.
fn skippable_blob(line: &str) -> bool {
    line.len() > MAX_LINE_BYTES
        && (line.contains("tool_result")
            || line.contains("tool_use")
            || line.contains("function_call"))
}

// Collapse a possibly-multiline message to one tidy line for a context lead-in.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// Join tool names, de-duplicated, order preserved.
fn dedup_join(tools: &[String]) -> String {
    let mut seen = Vec::new();
    for t in tools {
        if !seen.contains(&t.as_str()) {
            seen.push(t.as_str());
        }
    }
    seen.join(", ")
}

// The last non-empty path component (a project/cwd basename).
fn project_base(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

// Char-safe truncation that records how much was dropped, so the cap doesn't hide
// that the user often writes at length.
fn cap_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head} …[+{} chars truncated]", total - n)
}

// Pull the YYYY-MM-DD out of a `.../YYYY/MM/DD/rollout-YYYY-MM-DDTHH-...` path.
fn date_from_path(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if let Some(rest) = name.strip_prefix("rollout-") {
        if rest.len() >= 10 {
            return rest[..10].to_string();
        }
    }
    "unknown-date".to_string()
}

// Recursively collect files matching `pred` (depth-capped so a surprise deep tree
// can't wander).
fn walk_files(dir: &Path, depth: u32, out: &mut Vec<PathBuf>, pred: &dyn Fn(&str) -> bool) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_files(&p, depth + 1, out, pred);
        } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if pred(name) {
                out.push(p);
            }
        }
    }
}

// Next `style-vNNN.md` version by scanning existing outputs; 1 if none.
fn next_version(dir: &Path) -> u32 {
    let mut max = 0u32;
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if let Some(rest) = name.strip_prefix("style-v") {
                    if let Some(num) = rest.strip_suffix(".md") {
                        if let Ok(v) = num.parse::<u32>() {
                            max = max.max(v);
                        }
                    }
                }
            }
        }
    }
    let claims = dir.join("claims");
    if let Ok(entries) = fs::read_dir(claims) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(rest) = name.strip_prefix("style-v") {
                    if let Some(num) = rest.strip_suffix(".claim") {
                        if let Ok(version) = num.parse::<u32>() {
                            max = max.max(version);
                        }
                    }
                }
            }
        }
    }
    max + 1
}

fn reserve_version(dir: &Path, run_id: &str) -> Result<u32> {
    let claims = dir.join("claims");
    fs::create_dir_all(&claims).context("create distill claims directory")?;
    state::restrict_to_owner(&claims)?;
    let mut version = next_version(dir);
    loop {
        let path = claims.join(format!("style-v{version:03}.claim"));
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(format!("run_id={run_id}\n").as_bytes())?;
                file.sync_all()?;
                let parent = fs::File::open(&claims)?;
                parent.sync_all()?;
                return Ok(version);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                version = version.checked_add(1).context("distill version overflow")?;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("reserve {}", path.display()));
            }
        }
    }
}

// FNV-1a → 8 hex chars. Deterministic, dependency-free; used only to mint an
// opaque per-chunk marker for read-coverage verification (not security).
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", h & 0xffff_ffff)
}

/// Make sure codex's config trusts `dir`, so the launched interactive session
/// runs autonomously instead of stalling on the first-run "Do you trust this
/// folder?" gate. codex only honors the *persisted* trust decision (an ephemeral
/// `-c projects."dir".trust_level` override does NOT suppress the TUI gate —
/// verified), so we write the same `[projects."dir"]` entry codex writes when you
/// click "Yes, remember". Idempotent: one entry, ever. Best-effort — on failure
/// the session still works, it just waits for a one-time approval.
pub fn ensure_trusted(dir: &Path) -> Result<()> {
    let cfg = state::codex_home_dir().join("config.toml");
    ensure_trusted_at(&cfg, dir)
}

fn toml_basic_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c) => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn config_lock_path(cfg: &Path) -> Result<PathBuf> {
    let parent = cfg
        .parent()
        .context("codex config has no parent directory")?;
    let name = cfg
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    Ok(parent.join(format!(".{name}.rail.lock")))
}

fn acquire_trust_lock(cfg: &Path) -> Result<TrustLock> {
    let thread = TRUST_THREAD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = config_lock_path(cfg)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("open codex config lock {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!(
            "codex config lock is not a regular file: {}",
            path.display()
        );
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err).with_context(|| format!("lock codex config {}", path.display()));
    }
    Ok(TrustLock {
        _thread: thread,
        _file: file,
    })
}

fn read_config(cfg: &Path) -> Result<Option<(String, u32)>> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(cfg)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("open {}", cfg.display())),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("codex config is not a regular file: {}", cfg.display());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read {}", cfg.display()))?;
    let text =
        String::from_utf8(bytes).with_context(|| format!("decode {} as UTF-8", cfg.display()))?;
    Ok(Some((text, meta.permissions().mode() & 0o777)))
}

fn private_config_mode(existing: Option<u32>) -> u32 {
    // Preserve private owner bits (including a deliberately read-only config),
    // but never carry group/other access into the replacement. A new or
    // nonsensically mode-000 file becomes the normal owner-readable/writable 0600.
    let owner = existing.unwrap_or(0o600) & 0o700;
    if owner == 0 {
        0o600
    } else {
        owner
    }
}

fn create_unique_config_file(cfg: &Path) -> Result<(PathBuf, fs::File)> {
    let parent = cfg
        .parent()
        .context("codex config has no parent directory")?;
    let name = cfg
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    for _ in 0..64 {
        let n = TRUST_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{name}.rail-tmp-{}-{n:x}", std::process::id()));
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err).with_context(|| format!("create {}", path.display())),
        }
    }
    bail!("could not allocate a unique codex config file")
}

fn is_project_header(line: &str, marker: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix(marker) else {
        return false;
    };
    let rest = rest.trim_start();
    rest.is_empty() || rest.starts_with('#')
}

enum ProjectTrustEdit {
    Missing,
    AlreadyTrusted,
    Updated(String),
}

fn trust_assignment_value(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("trust_level")
        .or_else(|| trimmed.strip_prefix("\"trust_level\""))?;
    let rest = rest.trim_start().strip_prefix('=')?;
    Some(rest.trim_start())
}

fn trusted_value(value: &str) -> bool {
    let value = value.trim_end_matches(['\r', '\n']);
    let rest = value
        .strip_prefix("\"trusted\"")
        .or_else(|| value.strip_prefix("'trusted'"));
    rest.is_some_and(|tail| {
        let tail = tail.trim_start();
        tail.is_empty() || tail.starts_with('#')
    })
}

fn edit_existing_project_trust(existing: &str, marker: &str) -> ProjectTrustEdit {
    let lines: Vec<_> = existing.split_inclusive('\n').collect();
    let Some(header) = lines
        .iter()
        .position(|line| is_project_header(line, marker))
    else {
        return ProjectTrustEdit::Missing;
    };
    let end = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .find(|(_, line)| line.trim_start().starts_with('['))
        .map(|(index, _)| index)
        .unwrap_or(lines.len());

    if let Some((assignment, value)) = lines
        .iter()
        .enumerate()
        .take(end)
        .skip(header + 1)
        .find_map(|(index, line)| trust_assignment_value(line).map(|value| (index, value)))
    {
        if trusted_value(value) {
            return ProjectTrustEdit::AlreadyTrusted;
        }
        let line = lines[assignment];
        let indent_len = line.len() - line.trim_start().len();
        let indent = &line[..indent_len];
        let newline = if line.ends_with("\r\n") {
            "\r\n"
        } else if line.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        let mut updated = String::with_capacity(existing.len() + 16);
        updated.extend(lines[..assignment].iter().copied());
        updated.push_str(indent);
        updated.push_str("trust_level = \"trusted\"");
        updated.push_str(newline);
        updated.extend(lines[assignment + 1..].iter().copied());
        return ProjectTrustEdit::Updated(updated);
    }

    let header_line = lines[header];
    let newline = if header_line.ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut updated = String::with_capacity(existing.len() + 30);
    updated.extend(lines[..=header].iter().copied());
    if !header_line.ends_with('\n') {
        updated.push_str(newline);
    }
    updated.push_str("trust_level = \"trusted\"");
    updated.push_str(newline);
    updated.extend(lines[header + 1..].iter().copied());
    ProjectTrustEdit::Updated(updated)
}

fn ensure_trusted_at(cfg: &Path, dir: &Path) -> Result<()> {
    let parent = cfg
        .parent()
        .context("codex config has no parent directory")?;
    let dir = dir
        .to_str()
        .context("cannot add a non-UTF-8 project path to Codex config")?;
    let marker = format!("[projects.{}]", toml_basic_string(dir));
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    state::restrict_to_owner(parent)?;
    let _lock = acquire_trust_lock(cfg)?;

    // The lock serializes every rail process. The small retry also notices a
    // non-cooperating Codex process that rewrites config.toml while rail is
    // preparing its replacement, instead of blindly clobbering that update.
    for _ in 0..4 {
        let snapshot = read_config(cfg)?;
        let (existing, old_mode) = snapshot
            .as_ref()
            .map(|(text, mode)| (text.clone(), Some(*mode)))
            .unwrap_or_else(|| (String::new(), None));
        let mode = private_config_mode(old_mode);
        let updated = match edit_existing_project_trust(&existing, &marker) {
            ProjectTrustEdit::AlreadyTrusted => {
                let file = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(cfg)
                    .with_context(|| format!("open {}", cfg.display()))?;
                file.set_permissions(fs::Permissions::from_mode(mode))?;
                return Ok(());
            }
            ProjectTrustEdit::Updated(updated) => updated,
            ProjectTrustEdit::Missing => {
                let mut updated = existing;
                if !updated.is_empty() && !updated.ends_with('\n') {
                    updated.push('\n');
                }
                updated.push_str(&format!("\n{marker}\ntrust_level = \"trusted\"\n"));
                updated
            }
        };

        let (tmp, mut file) = create_unique_config_file(cfg)?;
        let cleanup = TrustTemp(tmp.clone());
        file.write_all(updated.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
        drop(file);

        if read_config(cfg)? != snapshot {
            drop(cleanup);
            continue;
        }
        let parent_dir = fs::File::open(parent)
            .with_context(|| format!("open codex config directory {}", parent.display()))?;
        fs::rename(&tmp, cfg).with_context(|| format!("install {}", cfg.display()))?;
        parent_dir
            .sync_all()
            .with_context(|| format!("sync codex config directory {}", parent.display()))?;
        drop(cleanup);
        return Ok(());
    }
    bail!("codex config kept changing while rail tried to update it")
}

/// The English instruction handed to the launched codex session. It must read
/// every chunk fully and echo back each chunk's marker id, so stale/partial
/// outputs can be rejected afterwards. Distills BOTH the user's voice and their problem-solving
/// logic, using the `[assistant]` context lines to interpret each user turn.
pub fn distill_prompt(prep: &DistillPrep) -> String {
    let n = prep.chunks.len();
    let out = &prep.output_file;
    let corpus = &prep.corpus_rel;
    format!(
        "You are studying the user's OWN past conversations to distill BOTH (a) how they \
write and (b) how they think and solve problems — their decision logic, not just their tone.\n\n\
The material is {sessions} of the user's richest real sessions ({codex} from the codex CLI, \
{claude} from Claude Code), split into {n} files in reading order:\n  \
{corpus}/corpus-01.md, {corpus}/corpus-02.md, …, {corpus}/corpus-{n:02}.md\n\n\
FORMAT of each file — a series of session transcripts. Within a session:\n  \
• lines beginning `[assistant]` are COMPRESSED CONTEXT: a short lead-in of what the assistant \
had just said or done (and which tools it ran). They exist only so the user's next turn is \
interpretable — do NOT study the assistant's style.\n  \
• lines beginning `> ` start the USER's OWN turns; `> | ` continues the same multiline turn \
(long ones truncated). THIS is the person you are distilling. Read each USER turn IN THE CONTEXT of the `[assistant]` line(s) \
above it: what were they reacting to, and what did they decide to do about it?\n\n\
Do the following IN ORDER and skip nothing:\n\n\
1. Read EVERY file {corpus}/corpus-01.md through {corpus}/corpus-{n:02}.md, IN FULL, top to bottom — do NOT grep, \
search, sample, or skim. Each file ends with a line `<<CHUNK k/{n} id=XXXX>>`. Record every id.\n\n\
2. As you read, study TWO things:\n  \
• VOICE — tone and directness; the Chinese/English code-switching; sentence shape and length; \
how they open a task vs. follow up; how they praise vs. push back; recurring phrases and tics.\n  \
• LOGIC — how they diagnose a problem; what they prioritize; how they decide the next step; when \
they demand verification, novelty, or evidence; what makes them approve (e.g. 继续/可以) vs. reject \
(e.g. 不对/重来); how they drive a task from opening to done; how much autonomy they grant; and \
the heuristics they repeat.\n\n\
3. Write a concise, evidence-based English profile to the file `{out}`, with these sections: \
Snapshot (3–5 sentences); Voice & tone; Language & code-switching; How they give instructions; \
How they give feedback / push back; Problem-solving logic & decision patterns; How they drive a \
task (open → steer → approve → close); What triggers approval vs. pushback; Recurring phrases & \
tics (quote short real snippets); Values & priorities; Do / Don't for imitating them (cover BOTH \
voice AND reasoning). Be specific — quote short real snippets, avoid generic filler.\n\n\
4. End `{out}` with this exact machine-readable footer: a line `## Coverage`; then exactly one \
line `CHUNK_ID=<id>` for EVERY chunk id recorded in step 1 (all {n}, no duplicates or extra ids); \
then exactly one line `USER_TURNS_READ={messages}`. The turn count is the {messages} `> ` USER \
turns in the prepared corpus. Do not vary these key names. This is Rail's completion contract.\n\n\
Write everything in English. When finished, confirm the file was written and how many chunks you \
covered.",
        sessions = prep.sessions,
        codex = prep.codex_sessions,
        claude = prep.claude_sessions,
        corpus = corpus,
        messages = prep.messages,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_blobs_and_empty() {
        assert!(is_real_user("看下我的 dataset，刷新 sota"));
        assert!(is_real_user("fix the bug, then re-run the tests please"));
        assert!(!is_real_user("   "));
        assert!(!is_real_user(
            "<subagent_notification> {\"agent_path\":\"x\"}"
        ));
        assert!(!is_real_user(
            "<environment_context>cwd=/tmp</environment_context>"
        ));
        assert!(!is_real_user(
            "<user_instructions>be nice</user_instructions>"
        ));
        // Claude-side blobs
        assert!(!is_real_user(
            "<local-command-caveat>Caveat: The messages below…"
        ));
        assert!(!is_real_user("<command-message>compact</command-message>"));
        assert!(!is_real_user(
            "You are analyzing the user's OWN past messages to distill…"
        ));
    }

    #[test]
    fn claude_content_str_and_blocks() {
        use serde_json::json;
        // bare string = human text
        let (t, tl) = extract_claude_content(Some(&json!("please rerun the eval")));
        assert_eq!(t, "please rerun the eval");
        assert!(tl.is_empty());
        // tool_result-only = injected, dropped to empty
        let (t, _) = extract_claude_content(Some(&json!([{"type":"tool_result","content":"OUT"}])));
        assert_eq!(t, "");
        // assistant text + tool_use → lead-in + tool names
        let (t, tl) = extract_claude_content(Some(&json!([
            {"type":"text","text":"I'll edit the file"},
            {"type":"tool_use","name":"Edit","input":{}},
            {"type":"tool_use","name":"Bash","input":{}}
        ])));
        assert_eq!(t, "I'll edit the file");
        assert_eq!(tl, vec!["Edit", "Bash"]);
    }

    #[test]
    fn format_convo_shows_user_and_context() {
        let c = Convo {
            src: Src::Claude,
            date: "2026-07-06".into(),
            project: "37_codex_rail".into(),
            turns: vec![
                Turn {
                    role: Role::Assistant,
                    text: "I ran the tests and 2 failed".into(),
                    tools: vec!["Bash".into(), "Bash".into()],
                },
                Turn {
                    role: Role::User,
                    text: "说人话，直接告诉我哪里错了".into(),
                    tools: vec![],
                },
            ],
        };
        let s = format_convo(&c, 1, 700, 220);
        assert!(s.contains("CLAUDE SESSION 1"));
        assert!(s.contains("37_codex_rail"));
        assert!(s.contains("[assistant] I ran the tests and 2 failed · did: Bash")); // deduped tools
        assert!(s.contains("> 说人话，直接告诉我哪里错了"));
    }

    #[test]
    fn clip_keeps_head_and_tail() {
        let turns: Vec<Turn> = (0..100)
            .map(|i| Turn {
                role: Role::User,
                text: format!("m{i}"),
                tools: vec![],
            })
            .collect();
        let kept = clip_turns(&turns, 9);
        assert_eq!(kept.len(), 9);
        assert_eq!(kept[0].text, "m0"); // head preserved
        assert_eq!(kept.last().unwrap().text, "m99"); // tail preserved
    }

    #[test]
    fn cap_keeps_head_and_notes_drop() {
        let s: String = "a".repeat(1000);
        let c = cap_chars(&s, 700);
        assert!(c.starts_with(&"a".repeat(700)));
        assert!(c.contains("+300 chars truncated"));
        assert_eq!(cap_chars("hi", 700), "hi");
        assert_eq!(cap_chars("你好世界", 2), "你好 …[+2 chars truncated]");
    }

    #[test]
    fn project_base_basename() {
        assert_eq!(project_base("/data/x/37_codex_rail"), "37_codex_rail");
        assert_eq!(project_base("/data/x/37_codex_rail/"), "37_codex_rail");
        assert_eq!(project_base(""), "");
    }

    #[test]
    fn version_numbering() {
        let dir = std::env::temp_dir().join(format!("rail-distill-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(next_version(&dir), 1);
        fs::write(dir.join("style-v001.md"), "x").unwrap();
        fs::write(dir.join("style-v004.md"), "x").unwrap();
        fs::write(dir.join("notes.md"), "x").unwrap();
        assert_eq!(next_version(&dir), 5);
        fs::create_dir_all(dir.join("claims")).unwrap();
        fs::write(dir.join("claims/style-v009.claim"), "run_id=test\n").unwrap();
        assert_eq!(next_version(&dir), 10);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn coverage_contract_requires_exact_markers_and_turn_count() {
        let dir = std::env::temp_dir().join(format!(
            "rail-distill-contract-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        fs::create_dir_all(&dir).unwrap();
        let output = dir.join("style-v001.md");
        let markers = vec!["a1b2c3d4".to_string(), "ffeedd00".to_string()];
        fs::write(
            &output,
            "# profile\n\n## Coverage\nCHUNK_ID=a1b2c3d4\nCHUNK_ID=ffeedd00\nUSER_TURNS_READ=7\n",
        )
        .unwrap();
        validate_output_contract(&output, &markers, 7).unwrap();

        fs::write(
            &output,
            "# profile\n## Coverage\nCHUNK_ID=a1b2c3d4\nUSER_TURNS_READ=7\n",
        )
        .unwrap();
        assert!(validate_output_contract(&output, &markers, 7).is_err());
        fs::write(
            &output,
            "# profile\n## Coverage\nCHUNK_ID=a1b2c3d4\nCHUNK_ID=ffeedd00\nCHUNK_ID=ffeedd00\nUSER_TURNS_READ=7\n",
        )
        .unwrap();
        assert!(validate_output_contract(&output, &markers, 7).is_err());
        fs::write(
            &output,
            "# profile\n## Coverage\nCHUNK_ID=a1b2c3d4\nCHUNK_ID=ffeedd00\nUSER_TURNS_READ=8\n",
        )
        .unwrap();
        assert!(validate_output_contract(&output, &markers, 7).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn inherited_run_lock_must_share_or_acquire_the_exclusive_flock() {
        let path = std::env::temp_dir().join(format!(
            "rail-distill-inherited-lock-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let owner = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        // dup/try_clone shares the locked open-file description and is valid.
        let inherited = owner.try_clone().unwrap();
        claim_inherited_run_lock(&inherited).unwrap();

        // A fresh open of the same trusted inode is not proof of ownership.
        let separate = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(claim_inherited_run_lock(&separate).is_err());
        drop(inherited);
        drop(owner);

        // Once the true owner releases, claiming the fresh descriptor is safe.
        claim_inherited_run_lock(&separate).unwrap();
        drop(separate);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn validated_style_requires_exact_private_regular_artifacts() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "rail-validated-style-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let style = dir.join("style-v007.md");
        let marker = dir.join("style-v007.validated");
        fs::write(&style, b"# style\n").unwrap();
        fs::write(&marker, b"wrong\n").unwrap();
        fs::set_permissions(&style, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(validated_style_file_at(&dir, 7).is_none());

        fs::write(&marker, b"validated-by-codex-rail\n").unwrap();
        assert_eq!(validated_style_file_at(&dir, 7), Some(style.clone()));
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validated_style_file_at(&dir, 7).is_none());
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();

        let victim = dir.join("victim");
        fs::write(&victim, b"not a style\n").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(&style).unwrap();
        symlink(&victim, &style).unwrap();
        assert!(validated_style_file_at(&dir, 7).is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn multiline_user_turns_have_unambiguous_continuations() {
        let convo = Convo {
            src: Src::Codex,
            date: "2026-07-13".to_string(),
            project: "rail".to_string(),
            turns: vec![Turn {
                role: Role::User,
                text: "first\n> quoted\nthird".to_string(),
                tools: Vec::new(),
            }],
        };
        let rendered = format_convo(&convo, 1, 700, 220);
        assert!(rendered.contains("> first\n> | > quoted\n> | third\n"));
        assert_eq!(
            clip_turns(&convo.turns, TURN_CAP)
                .into_iter()
                .filter(|turn| matches!(turn.role, Role::User))
                .count(),
            1
        );
    }

    #[test]
    fn date_parsing() {
        let p = std::path::Path::new("/x/2026/07/03/rollout-2026-07-03T22-45-01-uuid.jsonl");
        assert_eq!(date_from_path(p), "2026-07-03");
    }

    #[test]
    fn marker_is_stable_and_8_hex() {
        let m = short_hash("123:1");
        assert_eq!(m.len(), 8);
        assert!(m.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(short_hash("123:1"), short_hash("123:1"));
        assert_ne!(short_hash("123:1"), short_hash("123:2"));
    }

    #[test]
    fn toml_project_keys_escape_quotes_backslashes_and_controls() {
        assert_eq!(
            toml_basic_string("a\"b\\c\n\t\u{7f}"),
            "\"a\\\"b\\\\c\\n\\t\\u007f\""
        );
    }

    #[test]
    fn ensure_trusted_preserves_content_and_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "rail-trust-private-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        let cfg = dir.join(".codex/config.toml");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(&cfg, "model = \"gpt-test\"\n").unwrap();
        fs::set_permissions(&cfg, fs::Permissions::from_mode(0o600)).unwrap();
        let project = Path::new("/tmp/a\"quote\\tail\nnext");

        ensure_trusted_at(&cfg, project).unwrap();
        let content = fs::read_to_string(&cfg).unwrap();
        assert!(content.starts_with("model = \"gpt-test\"\n"));
        assert!(content.contains("[projects.\"/tmp/a\\\"quote\\\\tail\\nnext\"]"));
        assert_eq!(
            fs::metadata(&cfg).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(cfg.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_commented_project_table_does_not_count_as_trusted() {
        let dir = std::env::temp_dir().join(format!(
            "rail-trust-commented-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        let cfg = dir.join("config.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &cfg,
            "# [projects.\"/tmp/project\"]\n# trust_level = \"trusted\"\n",
        )
        .unwrap();

        ensure_trusted_at(&cfg, Path::new("/tmp/project")).unwrap();
        let content = fs::read_to_string(&cfg).unwrap();
        assert_eq!(content.matches("[projects.\"/tmp/project\"]").count(), 2);
        assert!(content
            .lines()
            .any(|line| line == "[projects.\"/tmp/project\"]"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn project_table_with_an_inline_comment_is_recognized() {
        let dir = std::env::temp_dir().join(format!(
            "rail-trust-inline-comment-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        let cfg = dir.join("config.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &cfg,
            "[projects.\"/tmp/project\"] # managed by the user\ntrust_level = \"untrusted\"\n",
        )
        .unwrap();

        ensure_trusted_at(&cfg, Path::new("/tmp/project")).unwrap();
        let content = fs::read_to_string(&cfg).unwrap();
        assert_eq!(content.matches("[projects.\"/tmp/project\"]").count(), 1);
        assert!(content.contains("# managed by the user"));
        assert!(content.contains("trust_level = \"trusted\""));
        assert!(!content.contains("trust_level = \"untrusted\""));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn non_utf8_project_path_fails_closed_without_touching_config() {
        use std::os::unix::ffi::OsStringExt;
        let dir = std::env::temp_dir().join(format!(
            "rail-trust-non-utf8-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        let cfg = dir.join("config.toml");
        fs::create_dir_all(&dir).unwrap();
        let original = "model = \"gpt-test\"\n";
        fs::write(&cfg, original).unwrap();
        let project = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/bad-\xff".to_vec()));

        assert!(ensure_trusted_at(&cfg, &project).is_err());
        assert_eq!(fs::read_to_string(&cfg).unwrap(), original);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ensure_trusted_preserves_private_owner_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "rail-trust-mode-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        let cfg = dir.join("config.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&cfg, "model = \"gpt-test\"\n").unwrap();
        fs::set_permissions(&cfg, fs::Permissions::from_mode(0o400)).unwrap();

        ensure_trusted_at(&cfg, Path::new("/tmp/project")).unwrap();
        assert_eq!(
            fs::metadata(&cfg).unwrap().permissions().mode() & 0o777,
            0o400
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ensure_trusted_never_turns_a_read_error_into_an_empty_config() {
        let dir = std::env::temp_dir().join(format!(
            "rail-trust-read-error-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        let cfg = dir.join("config.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&cfg, [0xff, 0xfe, 0xfd]).unwrap();

        assert!(ensure_trusted_at(&cfg, Path::new("/tmp/project")).is_err());
        assert_eq!(fs::read(&cfg).unwrap(), [0xff, 0xfe, 0xfd]);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_trust_updates_are_idempotent_and_do_not_clobber() {
        let dir = std::env::temp_dir().join(format!(
            "rail-trust-race-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        let cfg = std::sync::Arc::new(dir.join("config.toml"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(&*cfg, "model = \"gpt-test\"\n").unwrap();

        let threads: Vec<_> = ["/tmp/project-a", "/tmp/project-b"]
            .into_iter()
            .map(|project| {
                let cfg = cfg.clone();
                std::thread::spawn(move || ensure_trusted_at(&cfg, Path::new(project)).unwrap())
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let content = fs::read_to_string(&*cfg).unwrap();
        assert_eq!(content.matches("model = \"gpt-test\"").count(), 1);
        assert_eq!(content.matches("[projects.\"/tmp/project-a\"]").count(), 1);
        assert_eq!(content.matches("[projects.\"/tmp/project-b\"]").count(), 1);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_trust_updates_across_processes_do_not_clobber() {
        const CHILD_CFG: &str = "CODEX_RAIL_TEST_TRUST_CFG";
        const CHILD_PROJECT: &str = "CODEX_RAIL_TEST_TRUST_PROJECT";
        const CHILD_READY: &str = "CODEX_RAIL_TEST_TRUST_READY";
        const CHILD_GO: &str = "CODEX_RAIL_TEST_TRUST_GO";

        if let Some(cfg) = std::env::var_os(CHILD_CFG) {
            let project = std::env::var_os(CHILD_PROJECT).unwrap();
            let ready = PathBuf::from(std::env::var_os(CHILD_READY).unwrap());
            let go = PathBuf::from(std::env::var_os(CHILD_GO).unwrap());
            fs::write(&ready, b"ready").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !go.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "parent barrier timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            ensure_trusted_at(Path::new(&cfg), Path::new(&project)).unwrap();
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "rail-trust-process-race-{}-{}",
            std::process::id(),
            state::now_millis()
        ));
        let cfg = dir.join("config.toml");
        let go = dir.join("go");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&cfg, "model = \"gpt-test\"\n").unwrap();
        let harness = std::env::current_exe().unwrap();
        let spawn = |project: &str, ready: &Path| {
            std::process::Command::new(&harness)
                .args([
                    "--exact",
                    "distill::tests::concurrent_trust_updates_across_processes_do_not_clobber",
                ])
                .env(CHILD_CFG, &cfg)
                .env(CHILD_PROJECT, project)
                .env(CHILD_READY, ready)
                .env(CHILD_GO, &go)
                .spawn()
                .unwrap()
        };
        let ready_a = dir.join("ready-a");
        let ready_b = dir.join("ready-b");
        let mut child_a = spawn("/tmp/project-a", &ready_a);
        let mut child_b = spawn("/tmp/project-b", &ready_b);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready_a.exists() || !ready_b.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "child barrier timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        fs::write(&go, b"go").unwrap();
        assert!(child_a.wait().unwrap().success());
        assert!(child_b.wait().unwrap().success());

        let content = fs::read_to_string(&cfg).unwrap();
        assert_eq!(content.matches("model = \"gpt-test\"").count(), 1);
        assert_eq!(content.matches("[projects.\"/tmp/project-a\"]").count(), 1);
        assert_eq!(content.matches("[projects.\"/tmp/project-b\"]").count(), 1);

        let _ = fs::remove_dir_all(dir);
    }
}
