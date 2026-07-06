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
//! numbered chunks. A per-chunk marker the launched codex session must echo back
//! proves the read was complete, not sampled.

use crate::state;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::{cmp::Reverse, fs};

// ---- tunables (all env-overridable so tests can force small shapes) ----------

const MSG_CAP_DEFAULT: usize = 700; // chars kept per USER turn
const LEAD_CAP_DEFAULT: usize = 220; // chars kept per assistant lead-in (context)
const CHUNK_BYTES_DEFAULT: usize = 200_000; // per corpus chunk
const BUDGET_BYTES_DEFAULT: usize = 4_000_000; // total corpus size target
const SCAN_FILES_DEFAULT: usize = 320; // most-recent files scanned per source
const SCAN_BYTES_DEFAULT: usize = 300 * 1024 * 1024; // cap total bytes read per source
const HUGE_FILE_BYTES: u64 = 30 * 1024 * 1024; // skip a pathologically large transcript
const TURN_CAP: usize = 64; // max turns kept per session (head + tail if longer)
// A line this long is only ever a giant tool_call/tool_result blob — a human turn
// or an assistant lead-in never approaches it. Skipping such lines (see
// skippable_blob) before JSON-parsing is what keeps prepare() fast on multi-MB
// transcripts, WITHOUT dropping the user's long pasted turns (those lack the tool
// markers, so they're kept and capped later).
const MAX_LINE_BYTES: usize = 24_000;

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

pub struct DistillPrep {
    pub workdir: PathBuf,     // distill_dir(); the launched session's cwd
    pub version: u32,         // next style version (1-based)
    pub output_file: String,  // "style-v001.md", relative to workdir
    pub sessions: usize,      // sessions included in the corpus
    pub messages: usize,      // user turns included
    pub codex_sessions: usize, // of `sessions`, how many are from codex
    pub claude_sessions: usize, // …and from Claude Code
    pub scanned: usize,       // transcript files actually read
    pub available: usize,     // sessions with >=1 real user turn found while scanning
    pub chunks: Vec<Chunk>,
}

/// Aggregate the archives into `distill_dir()/corpus/` and return a plan the
/// launcher turns into a codex session. Regenerates the corpus every call.
pub fn prepare() -> Result<DistillPrep> {
    let workdir = state::distill_dir();
    let corpus_dir = workdir.join("corpus");
    fs::create_dir_all(&corpus_dir).context("create distill corpus dir")?;
    // Clear any corpus from a previous run so stale chunks can't linger.
    if let Ok(entries) = fs::read_dir(&corpus_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "md").unwrap_or(false) {
                let _ = fs::remove_file(&p);
            }
        }
    }

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

    // Greedily format the richest sessions until the byte budget is filled.
    let budget = budget_bytes();
    let mut body_blocks: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut included = 0usize;
    let mut user_msgs = 0usize;
    let (mut inc_codex, mut inc_claude) = (0usize, 0usize);
    for c in &convos {
        if total >= budget {
            break;
        }
        let block = format_convo(c, included + 1, cap, lead);
        total += block.len();
        user_msgs += c.user_turns();
        match c.src {
            Src::Codex => inc_codex += 1,
            Src::Claude => inc_claude += 1,
        }
        included += 1;
        body_blocks.push(block);
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
    let salt = state::now_millis();
    let mut chunks = Vec::with_capacity(n);
    for (i, body) in buffers.into_iter().enumerate() {
        let k = i + 1;
        let file = format!("corpus-{k:02}.md");
        let marker = short_hash(&format!("{salt}:{k}"));
        let content = format!(
            "# corpus chunk {k}/{n} — the user's own conversations, in context (read this file fully)\n{body}\n<<CHUNK {k}/{n} id={marker}>>\n"
        );
        fs::write(corpus_dir.join(&file), content)
            .with_context(|| format!("write corpus chunk {file}"))?;
        chunks.push(Chunk { file, marker });
    }

    let version = next_version(&workdir);
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
    })
}

// Scan both sources (most-recent files first) into contextualized conversations.
// Returns the convos with >=1 real user turn and how many files were read.
fn scan_all() -> (Vec<Convo>, usize) {
    let cap = scan_files();
    let mut convos = Vec::new();
    let mut scanned = 0;
    scanned += scan_codex(cap, &mut convos);
    scanned += scan_claude(cap, &mut convos);
    convos.retain(|c| c.user_turns() > 0);
    (convos, scanned)
}

// codex rollouts: user_message (user), agent_message (assistant lead-in),
// function_call (tool names attached to the surrounding assistant turn).
fn scan_codex(cap: usize, out: &mut Vec<Convo>) -> usize {
    let mut files = Vec::new();
    walk_files(&state::codex_sessions_dir(), 0, &mut files, &|n| {
        n.starts_with("rollout-") && n.ends_with(".jsonl")
    });
    // rollout-<ISO timestamp> filenames sort chronologically; newest first.
    files.sort();
    files.reverse();
    files.truncate(cap);

    let mut scanned = 0;
    let mut bytes = 0usize;
    for path in files {
        if bytes >= SCAN_BYTES_DEFAULT {
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
    // Newest first by mtime (Claude filenames are random UUIDs, not sortable).
    files.sort_by_key(|p| Reverse(fs::metadata(p).and_then(|m| m.modified()).ok()));
    files.truncate(cap);

    let mut scanned = 0;
    let mut bytes = 0usize;
    for path in files {
        if bytes >= SCAN_BYTES_DEFAULT {
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
    let proj = if c.project.is_empty() { "-" } else { &c.project };
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
                s.push_str("> ");
                s.push_str(&cap_chars(t.text.trim(), cap));
                s.push('\n');
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
    max + 1
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
    let marker = format!("[projects.\"{}\"]", dir.to_string_lossy());
    let existing = fs::read_to_string(&cfg).unwrap_or_default();
    if existing.contains(&marker) {
        return Ok(()); // already trusted
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("\n{marker}\ntrust_level = \"trusted\"\n"));
    if let Some(parent) = cfg.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Write atomically (temp + rename) so an interrupted write can't corrupt the
    // user's codex config.
    let tmp = cfg.with_extension("toml.rail-tmp");
    fs::write(&tmp, &updated).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &cfg).with_context(|| format!("install {}", cfg.display()))?;
    Ok(())
}

/// The English instruction handed to the launched codex session. It must read
/// every chunk fully and echo back each chunk's marker id, so completeness can be
/// checked afterwards. Distills BOTH the user's voice and their problem-solving
/// logic, using the `[assistant]` context lines to interpret each user turn.
pub fn distill_prompt(prep: &DistillPrep) -> String {
    let n = prep.chunks.len();
    let out = &prep.output_file;
    format!(
        "You are studying the user's OWN past conversations to distill BOTH (a) how they \
write and (b) how they think and solve problems — their decision logic, not just their tone.\n\n\
The material is {sessions} of the user's richest real sessions ({codex} from the codex CLI, \
{claude} from Claude Code), split into {n} files in reading order:\n  \
corpus/corpus-01.md, corpus/corpus-02.md, …, corpus/corpus-{n:02}.md\n\n\
FORMAT of each file — a series of session transcripts. Within a session:\n  \
• lines beginning `[assistant]` are COMPRESSED CONTEXT: a short lead-in of what the assistant \
had just said or done (and which tools it ran). They exist only so the user's next turn is \
interpretable — do NOT study the assistant's style.\n  \
• lines beginning `> ` are the USER's OWN turns, verbatim (long ones truncated). THIS is the \
person you are distilling. Read each `> ` turn IN THE CONTEXT of the `[assistant]` line(s) \
above it: what were they reacting to, and what did they decide to do about it?\n\n\
Do the following IN ORDER and skip nothing:\n\n\
1. Read EVERY file corpus-01.md through corpus-{n:02}.md, IN FULL, top to bottom — do NOT grep, \
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
4. End `{out}` with a `## Coverage` section that lists EVERY chunk id you recorded in step 1 \
(all {n}, one per line) and the total number of USER turns you actually read. This verifies a \
complete read.\n\n\
Write everything in English. When finished, confirm the file was written and how many chunks you \
covered.",
        sessions = prep.sessions,
        codex = prep.codex_sessions,
        claude = prep.claude_sessions,
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
        assert!(!is_real_user("<subagent_notification> {\"agent_path\":\"x\"}"));
        assert!(!is_real_user("<environment_context>cwd=/tmp</environment_context>"));
        assert!(!is_real_user("<user_instructions>be nice</user_instructions>"));
        // Claude-side blobs
        assert!(!is_real_user("<local-command-caveat>Caveat: The messages below…"));
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
        let _ = fs::remove_dir_all(&dir);
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
}
