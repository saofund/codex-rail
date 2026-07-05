//! Archive distillation.
//!
//! Reads the user's OWN past messages out of codex's rollout transcripts
//! (`~/.codex/sessions/**/rollout-*.jsonl`), filters them down to genuine
//! human-typed prose, and writes them as a compact, **codex-readable** corpus
//! split into small numbered chunks. A launched codex session then reads the
//! whole corpus and summarizes the user's writing/response style into a
//! versioned `style-vNNN.md`.
//!
//! Why aggregate first: the raw rollouts are hundreds of MB (re-injected
//! context, tool output, reasoning). Handed that, codex would grep/sample and
//! never ingest it all. By pre-extracting just the user's messages into a few
//! small files, we make a *complete* read tractable — and verifiable, via a
//! per-chunk marker the codex session is told to echo back.

use crate::state;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

// Per-message char cap: a person's style lives in how they open and phrase
// things, not in the pasted files/logs that bloat long messages. We keep the
// head and annotate the dropped length so codex still learns "they write long".
// Overridable via env so tests can force different shapes.
const MSG_CAP_DEFAULT: usize = 700;
// Target chunk size: small enough that codex reads each file in a single pass
// rather than paginating (and possibly stopping early). Overridable via env so a
// tiny synthetic corpus can still exercise multi-chunk reading.
const CHUNK_BYTES_DEFAULT: usize = 200_000;

fn msg_cap() -> usize {
    env_usize("CODEX_RAIL_DISTILL_MSG_CAP", MSG_CAP_DEFAULT)
}
fn chunk_bytes() -> usize {
    env_usize("CODEX_RAIL_DISTILL_CHUNK_BYTES", CHUNK_BYTES_DEFAULT).max(200)
}
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

pub struct Chunk {
    pub file: String,   // e.g. "corpus-01.md" (relative to corpus/)
    pub marker: String, // the id echoed on this chunk's trailing <<CHUNK…>> line
}

pub struct DistillPrep {
    pub workdir: PathBuf,    // distill_dir(); the launched session's cwd
    pub version: u32,        // next style version (1-based)
    pub output_file: String, // "style-v001.md", relative to workdir
    pub sessions: usize,     // sessions contributing >=1 real user message
    pub messages: usize,     // total user messages aggregated
    pub chunks: Vec<Chunk>,
}

// One session's extracted messages, in chronological order.
struct SessionMsgs {
    date: String,
    msgs: Vec<String>,
}

/// Aggregate the archive into `distill_dir()/corpus/` and return a plan the
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

    let sessions = scan_sessions();
    let total_sessions = sessions.iter().filter(|s| !s.msgs.is_empty()).count();
    let total_messages: usize = sessions.iter().map(|s| s.msgs.len()).sum();

    // Build the corpus body as a list of blocks, then greedily pack blocks into
    // chunk-sized buffers. A session may span a chunk boundary — that's fine,
    // codex reads every chunk in order.
    let cap = msg_cap();
    let chunk_bytes = chunk_bytes();
    let mut buffers: Vec<String> = Vec::new();
    let mut cur = String::new();
    let push_block = |block: String, cur: &mut String, buffers: &mut Vec<String>| {
        if !cur.is_empty() && cur.len() + block.len() > chunk_bytes {
            buffers.push(std::mem::take(cur));
        }
        cur.push_str(&block);
    };
    let mut si = 0usize;
    for s in &sessions {
        if s.msgs.is_empty() {
            continue;
        }
        si += 1;
        push_block(
            format!(
                "\n===== SESSION {} · {} · {} message(s) =====\n",
                si,
                s.date,
                s.msgs.len()
            ),
            &mut cur,
            &mut buffers,
        );
        for (mi, m) in s.msgs.iter().enumerate() {
            push_block(
                format!("--- message {mi}. ---\n{}\n", cap_chars(m, cap)),
                &mut cur,
                &mut buffers,
            );
        }
    }
    if !cur.is_empty() {
        buffers.push(cur);
    }

    // Now the chunk count is known; write each file with a header and a trailing
    // marker line whose id is unique to this run (so a stale style file can't
    // pass verification).
    let n = buffers.len();
    let salt = state::now_millis();
    let mut chunks = Vec::with_capacity(n);
    for (i, body) in buffers.into_iter().enumerate() {
        let k = i + 1;
        let file = format!("corpus-{k:02}.md");
        let marker = short_hash(&format!("{salt}:{k}"));
        let content = format!(
            "# corpus chunk {k}/{n} — the user's own past messages (read this file fully)\n{body}\n<<CHUNK {k}/{n} id={marker}>>\n"
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
        sessions: total_sessions,
        messages: total_messages,
        chunks,
    })
}

// Walk every rollout file (sorted → chronological by the YYYY/MM/DD/rollout-ts
// path) and pull out the user's genuine messages. Best-effort: unreadable or
// malformed files/lines are skipped.
fn scan_sessions() -> Vec<SessionMsgs> {
    let mut files = Vec::new();
    walk_rollouts(&state::codex_sessions_dir(), 0, &mut files);
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let date = date_from_path(&path);
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let mut msgs = Vec::new();
        for line in content.lines() {
            // Cheap pre-filter: only user_message lines can carry a user turn,
            // so we skip parsing the ~99% that can't (agent/reasoning/tool).
            if !line.contains("\"user_message\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
                continue;
            }
            let payload = v.get("payload");
            if payload.and_then(|p| p.get("type")).and_then(|t| t.as_str()) != Some("user_message") {
                continue;
            }
            let text = payload
                .and_then(|p| p.get("message").or_else(|| p.get("text")))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if is_real_user(text) {
                msgs.push(text.trim().to_string());
            }
        }
        out.push(SessionMsgs { date, msgs });
    }
    out
}

// A user_message payload can also carry tool/system-injected blobs that surface
// as user-role text (a subagent notification, the first-turn environment
// context, etc.). Those aren't the human's voice, so drop them.
fn is_real_user(text: &str) -> bool {
    let s = text.trim();
    if s.is_empty() {
        return false;
    }
    const BLOBS: [&str; 8] = [
        "<subagent_notification>",
        "<system-reminder>",
        "<environment_context>",
        "<user_instructions>",
        "<INSTRUCTIONS>",
        "<local-command",
        "<command-name>",
        "IMPORTANT: this context",
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

// Char-safe truncation that records how much was dropped, so the cap doesn't
// hide the fact that the user often writes at length.
fn cap_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head} …[+{} chars truncated]", total - n)
}

// Pull the YYYY-MM-DD out of a `.../YYYY/MM/DD/rollout-YYYY-MM-DDTHH-...` path.
fn date_from_path(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    // rollout-2026-07-03T22-45-... → take the date after "rollout-"
    if let Some(rest) = name.strip_prefix("rollout-") {
        if rest.len() >= 10 {
            return rest[..10].to_string();
        }
    }
    "unknown-date".to_string()
}

// Recursively collect rollout-*.jsonl paths (codex nests them YYYY/MM/DD). Depth
// capped so a surprise deep tree can't wander.
fn walk_rollouts(dir: &std::path::Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_rollouts(&p, depth + 1, out);
        } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                out.push(p);
            }
        }
    }
}

// Next `style-vNNN.md` version by scanning existing outputs; 1 if none.
fn next_version(dir: &std::path::Path) -> u32 {
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
/// folder?" gate. codex only honors the *persisted* trust decision (an
/// ephemeral `-c projects."dir".trust_level` override does NOT suppress the TUI
/// gate — verified), so we write the same `[projects."dir"]` entry codex writes
/// when you click "Yes, remember". Idempotent: one entry, ever. Best-effort —
/// on failure the session still works, it just waits for a one-time approval.
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
/// every chunk fully and echo back each chunk's marker id, so completeness can
/// be checked afterwards.
pub fn distill_prompt(prep: &DistillPrep) -> String {
    let n = prep.chunks.len();
    let out = &prep.output_file;
    format!(
        "You are analyzing the user's OWN past messages to distill their personal \
writing and response style.\n\n\
Their real messages (from {sessions} past Codex sessions, {messages} messages total) have been \
extracted and split into {n} files, in chronological order:\n  \
corpus/corpus-01.md, corpus/corpus-02.md, …, corpus/corpus-{n:02}.md\n\n\
Do the following IN ORDER and skip nothing:\n\n\
1. Read EVERY corpus file from corpus-01.md through corpus-{n:02}.md, IN FULL, top to bottom. \
Read each file completely — do NOT grep, search, sample, or skim; the point is to ingest all of \
the user's messages. Each file ends with a line of the form `<<CHUNK k/{n} id=XXXX>>`. Record the \
id from every file.\n\n\
2. As you read, study HOW THE USER WRITES (not the topics): tone and directness; the mix of \
Chinese and English; sentence shape and length; how they open a task vs. how they follow up; how \
they give feedback and corrections (praise, pushback, impatience); recurring phrases and verbal \
tics; punctuation and formatting habits; and what they consistently value or demand.\n\n\
3. Write a concise, well-structured English summary of the user's response style to the file \
`{out}`, with these sections: Snapshot (3–5 sentence portrait); Voice & tone; Language & \
code-switching; How they give instructions; How they give feedback / push back; Recurring phrases \
& tics (quote short real snippets); Values & priorities; Do / Don't for imitating them. Be \
specific and evidence-based — quote short real snippets, avoid generic filler.\n\n\
4. End `{out}` with a `## Coverage` section that lists EVERY chunk id you recorded in step 1 (all \
{n} of them, one id per line) and the total number of user messages you actually read. This is \
used to verify you read the whole archive.\n\n\
Write everything in English. When finished, confirm the file was written and how many chunks you \
covered.",
        sessions = prep.sessions,
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
        assert!(!is_real_user("<subagent_notification> {\"agent_path\":\"x\"}"));
        assert!(!is_real_user("<environment_context>cwd=/tmp</environment_context>"));
        assert!(!is_real_user("<user_instructions>be nice</user_instructions>"));
    }

    #[test]
    fn cap_keeps_head_and_notes_drop() {
        let s: String = "a".repeat(1000);
        let c = cap_chars(&s, 700);
        assert!(c.starts_with(&"a".repeat(700)));
        assert!(c.contains("+300 chars truncated"));
        // short strings pass through untouched
        assert_eq!(cap_chars("hi", 700), "hi");
        // char-safe on multibyte
        assert_eq!(cap_chars("你好世界", 2), "你好 …[+2 chars truncated]");
    }

    #[test]
    fn version_numbering() {
        // uses a temp dir so it doesn't touch the real config
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
