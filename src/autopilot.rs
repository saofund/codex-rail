//! Autopilot: a per-session **pilot** — a real, visible codex session that rail
//! drives to answer a *main* session on the user's behalf while it waits for
//! input. Toggled with Space on the selected session.
//!
//! The pilot reads the user's distilled style plus the main session's latest
//! message and writes the user's next reply; rail injects that reply back into
//! the main session. Both directions ("drive the pilot" and "deliver the reply")
//! go through the ordinary attach socket (`protocol::write_input_frame`) — the
//! same bytes a human types. Nothing about codex is patched.
//!
//! State per main session lives in `jobs/<main_id>/autopilot.json` (manager-owned,
//! like the label). The pilot is an ordinary rail session linked back by its id.

use crate::protocol;
use crate::state;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

fn default_cap() -> u32 {
    // env override so a power user can loosen/tighten the human-checkin cadence
    std::env::var("CODEX_RAIL_AUTOPILOT_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8)
}

/// Where the pilot is in the reply cycle for its main session.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Phase {
    /// Watching the main session; fire when it completes a new turn.
    Idle,
    /// Pilot is producing a reply; waiting for its turn to finish.
    Generating,
    /// Reply is ready; trying to inject it into the main session (retries if a
    /// human is attached and the worker refuses our headless client).
    Delivering,
}

fn phase_idle() -> Phase {
    Phase::Idle
}

/// Persisted autopilot control for one main session.
#[derive(Clone, Serialize, Deserialize)]
pub struct AutopilotState {
    pub enabled: bool,
    #[serde(default)]
    pub pilot_id: Option<String>,
    #[serde(default)]
    pub replies: u32,
    #[serde(default = "default_cap")]
    pub cap: u32,
    #[serde(default = "phase_idle")]
    pub phase: Phase,
    /// The main's last agent message we're currently handling (edge-detects a new
    /// turn: when the live message differs from this, there's something new to
    /// answer). Empty until the first trigger.
    #[serde(default)]
    pub main_marker: String,
    /// The pilot's last agent message at the moment we asked it to reply — so we
    /// can tell its *new* reply apart from a stale one.
    #[serde(default)]
    pub pilot_marker: String,
    /// In `Delivering`: the reply text awaiting injection into the main session.
    #[serde(default)]
    pub pending_reply: String,
    /// Why autopilot paused/handed back (shown to the user).
    #[serde(default)]
    pub last_reason: Option<String>,
}

impl Default for AutopilotState {
    fn default() -> Self {
        AutopilotState {
            enabled: false,
            pilot_id: None,
            replies: 0,
            cap: default_cap(),
            phase: Phase::Idle,
            main_marker: String::new(),
            pilot_marker: String::new(),
            pending_reply: String::new(),
            last_reason: None,
        }
    }
}

pub fn path(main_id: &str) -> PathBuf {
    state::job_dir(main_id).join("autopilot.json")
}

pub fn load(main_id: &str) -> Option<AutopilotState> {
    let s = fs::read_to_string(path(main_id)).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn save(main_id: &str, st: &AutopilotState) {
    let p = path(main_id);
    if let Ok(bytes) = serde_json::to_vec_pretty(st) {
        let _ = state::write_private_file(&p, bytes);
    }
}

pub fn remove(main_id: &str) {
    let _ = fs::remove_file(path(main_id));
}

/// Send `bytes` to a session's PTY via the ordinary attach socket, headlessly —
/// exactly the bytes a human would type. Returns `Ok(false)` when a human is
/// already attached (the worker refuses a second client) so the caller can retry
/// later; `Ok(true)` once delivered.
pub fn inject(socket: &str, bytes: &[u8]) -> std::io::Result<bool> {
    let mut stream = UnixStream::connect(socket)?;
    write!(stream, "ATTACH 24 80\n")?;
    stream.flush()?;
    // The worker sends "already attached elsewhere" immediately (before any log
    // tail) and drops us if a human holds the session.
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok();
    let mut buf = [0_u8; 512];
    if let Ok(n) = stream.read(&mut buf) {
        if n > 0 && String::from_utf8_lossy(&buf[..n]).contains("already attached") {
            return Ok(false);
        }
    }
    stream.set_read_timeout(None).ok();
    protocol::write_input_frame(&mut stream, bytes)?;
    // Let the worker forward to the PTY before we drop the client.
    std::thread::sleep(Duration::from_millis(120));
    let _ = protocol::write_detach_frame(&mut stream);
    Ok(true)
}

/// The pilot's initial task, handed to it on its first spawn: who it is, where
/// the user's distilled style lives, which session it is answering and where that
/// session's transcript is, and the strict output contract (reply text only, or a
/// `[HANDBACK] reason` / `[DONE]` sentinel).
pub fn initial_prompt(
    style_path: &str,
    main_id: &str,
    main_rollout: &str,
    main_last_msg: &str,
) -> String {
    format!(
        "You are the AUTO-REPLY PILOT for another codex session the user is running. \
Your job is to write the user's next reply on their behalf, in their voice and with their judgment, \
so their session keeps moving while they're away.\n\n\
The user's response style and problem-solving logic (distilled from their real history) is in this file — read it first:\n  {style}\n\n\
You are answering session id `{id}`. Its full transcript is here if you need more context:\n  {rollout}\n\n\
Each time I say \"看最新回复，继续\", read that session's latest state and write the user's next reply.\n\n\
RIGHT NOW, the session just finished a turn and said:\n---\n{last}\n---\n\n\
OUTPUT CONTRACT — obey exactly:\n\
- Output ONLY the user's next reply (the raw text to send), nothing else. No preamble, no explanation, no quotes.\n\
- Match the user's voice (their language, their brevity, their steering).\n\
- If the session ASKS you a question or offers a choice, ANSWER it — do not stop.\n\
- Output exactly [DONE] ONLY when the whole task is finished AND the session is not asking you anything.\n\
- If something risky/irreversible/destructive is proposed, or you are genuinely unsure what the user would want, output exactly: [HANDBACK] <one-line reason>\n\
Write the reply now.",
        style = style_path,
        id = main_id,
        rollout = main_rollout,
        last = clip(main_last_msg, 4000),
    )
}

/// The nudge for a pilot that already exists: answer the main session's newest
/// turn. Includes the latest message directly (so the pilot needn't re-read the
/// whole transcript) while still pointing at the file for deeper context.
pub fn continue_prompt(main_last_msg: &str) -> String {
    format!(
        "看最新回复，继续。The session just said:\n---\n{last}\n---\n\
Write the user's next reply now (in their voice). If it asks a question or offers a choice, ANSWER it. \
Reply text ONLY — or [DONE] only if finished and not being asked anything, or [HANDBACK] <reason>.",
        last = clip(main_last_msg, 4000)
    )
}

/// What the pilot's reply means.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// Send this text to the main session.
    Send(String),
    /// Task done — stop autopilot quietly.
    Done,
    /// Hand back to the human with this reason.
    HandBack(String),
}

/// Interpret the pilot's final message: sentinel first, else the reply text.
pub fn parse_reply(pilot_msg: &str) -> Reply {
    let t = pilot_msg.trim();
    if t.is_empty() {
        return Reply::HandBack("pilot produced an empty reply".to_string());
    }
    // Sentinels may appear on their own or lead the message.
    if let Some(rest) = t.strip_prefix("[HANDBACK]") {
        return Reply::HandBack(trim_reason(rest));
    }
    if t == "[DONE]" || t.starts_with("[DONE]") {
        return Reply::Done;
    }
    // A defensive guard: if the pilot leaked a sentinel mid-text, treat the whole
    // thing as a handback rather than sending a malformed reply.
    if t.contains("[HANDBACK]") {
        return Reply::HandBack("pilot was unsure (embedded handback)".to_string());
    }
    Reply::Send(t.to_string())
}

fn trim_reason(s: &str) -> String {
    let r = s.trim().trim_start_matches([':', '-', ' ']).trim();
    if r.is_empty() {
        "pilot handed back".to_string()
    } else {
        clip(r, 200)
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// The full text of the newest codex agent message in a rollout (not the
/// first-line preview) — used to extract the pilot's reply. Skips codex's
/// synthetic markers, mirroring the manager's preview derivation.
pub fn last_agent_message_full(rollout_path: &str) -> Option<String> {
    let mut file = fs::File::open(rollout_path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(p) = v.get("payload") else { continue };
        if p.get("type").and_then(|t| t.as_str()) != Some("agent_message") {
            continue;
        }
        if let Some(m) = p.get("message").and_then(|m| m.as_str()) {
            if state::is_synthetic_marker(m) {
                continue;
            }
            let m = m.trim();
            if !m.is_empty() {
                return Some(m.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reply_sentinels_and_text() {
        assert_eq!(parse_reply("  先查重，别急  "), Reply::Send("先查重，别急".to_string()));
        assert_eq!(parse_reply("[DONE]"), Reply::Done);
        assert_eq!(parse_reply("[DONE] task finished"), Reply::Done);
        match parse_reply("[HANDBACK] proposes rm -rf") {
            Reply::HandBack(r) => assert!(r.contains("rm -rf")),
            other => panic!("expected handback, got {other:?}"),
        }
        // empty -> handback, never an empty send
        assert!(matches!(parse_reply("   "), Reply::HandBack(_)));
        // leaked sentinel mid-text -> handback, not a malformed send
        assert!(matches!(parse_reply("sure, go ahead [HANDBACK] wait"), Reply::HandBack(_)));
    }

    #[test]
    fn default_cap_is_positive() {
        assert!(AutopilotState::default().cap >= 1);
        assert!(!AutopilotState::default().enabled);
        assert_eq!(AutopilotState::default().phase, Phase::Idle);
    }

    #[test]
    fn clip_respects_char_boundaries() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abcdef", 3), "abc…");
        // multibyte-safe (no panic on a CJK boundary)
        assert_eq!(clip("你好世界", 2), "你好…");
    }
}
