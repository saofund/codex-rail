use crate::protocol::{self, ClientFrame};
use crate::state::{
    self, SessionState, STATUS_EXITED, STATUS_FAILED, STATUS_RUNNING, STATUS_STOPPING,
};
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;
const TAIL_BYTES: usize = 96 * 1024;

pub fn run_worker(id: &str) -> Result<()> {
    unsafe {
        libc::setsid();
    }

    match run_worker_inner(id) {
        Ok(()) => Ok(()),
        Err(err) => {
            mark_failed(id, &format!("{err:#}"));
            Err(err)
        }
    }
}

fn run_worker_inner(id: &str) -> Result<()> {
    state::ensure_base_dirs()?;
    let mut session = state::read_state(id)?;
    let socket_path = state::socket_path(id);
    if socket_path.exists() {
        fs::remove_file(&socket_path).ok();
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    state::restrict_file_to_owner(&socket_path)?;
    listener
        .set_nonblocking(true)
        .context("set listener nonblocking")?;

    session.socket = socket_path.to_string_lossy().to_string();
    session.worker_pid = Some(process::id());
    session.status = STATUS_RUNNING.to_string();
    session.updated_at = state::now_secs();
    state::write_state(&session)?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: INITIAL_ROWS,
            cols: INITIAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open pty")?;

    let mut cmd = CommandBuilder::new(&session.codex);
    cmd.cwd(Path::new(&session.cwd));
    cmd.env("TERM", "xterm-256color");
    // Consume the first-message prompt exactly once: take() clears it from the
    // in-memory session so the write_state below persists None and a later
    // resume/restart won't replay it.
    let initial_prompt = session.initial_prompt.take();
    let is_resume = match &session.codex_session_id {
        Some(codex_session_id) => {
            cmd.arg("resume");
            cmd.arg(codex_session_id);
            true
        }
        None => {
            // Fresh session: hand codex the first message as a positional
            // prompt so it starts the first turn on spawn (interactive TUI).
            // codex writes its rollout as soon as that turn begins, which is
            // what lets the manager capture the path and read accurate status.
            if let Some(prompt) = &initial_prompt {
                cmd.arg(prompt);
            }
            false
        }
    };

    // Snapshot codex's existing rollout files BEFORE spawning, so the watcher
    // can tell which file this specific child creates. Snapshotting after the
    // spawn would race: codex can write its rollout file within the few
    // milliseconds before the watcher thread starts, so it would look
    // pre-existing and never be detected as new.
    let codex_sessions_before: Option<HashSet<PathBuf>> = if is_resume {
        None
    } else {
        let mut before = Vec::new();
        walk_jsonl(&state::codex_sessions_dir(), 0, &mut before);
        Some(before.into_iter().collect())
    };

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawn {}", session.codex))?;
    let child_pid = child.process_id();
    drop(pair.slave);

    session.child_pid = child_pid;
    session.updated_at = state::now_secs();
    state::write_state(&session)?;

    let pty_reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let mut pty_writer = pair.master.take_writer().context("take pty writer")?;
    let log_path = state::log_path(id);
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("open output log")?;
    state::restrict_file_to_owner(&log_path)?;

    let (tx, rx) = mpsc::channel();
    spawn_pty_reader(pty_reader, tx.clone());
    {
        let tx = tx.clone();
        thread::spawn(move || {
            let _ = child.wait();
            tx.send(WorkerEvent::ChildExit).ok();
        });
    }
    if let Some(before) = codex_sessions_before {
        spawn_session_id_watcher(tx.clone(), before);
    }

    let mut attached: Option<(u64, UnixStream)> = None;
    let mut next_client_id = 1_u64;
    let mut stop_requested_at: Option<Instant> = None;
    let mut last_output_persisted_at: Option<Instant> = None;
    const OUTPUT_PERSIST_INTERVAL: Duration = Duration::from_secs(2);

    loop {
        accept_connections(
            &listener,
            &tx,
            &mut attached,
            &mut next_client_id,
            &*pair.master,
            &session,
        )?;

        match rx.recv_timeout(Duration::from_millis(80)) {
            Ok(WorkerEvent::PtyOutput(bytes)) => {
                log.write_all(&bytes).ok();
                log.flush().ok();
                if let Some((_, client)) = attached.as_mut() {
                    if client
                        .write_all(&bytes)
                        .and_then(|_| client.flush())
                        .is_err()
                    {
                        attached = None;
                    }
                }
                // Coarse busy/idle signal for the manager UI: throttled so a
                // fast-streaming codex response doesn't turn into a
                // write_state call per PTY chunk.
                let should_persist = last_output_persisted_at
                    .map(|at| at.elapsed() >= OUTPUT_PERSIST_INTERVAL)
                    .unwrap_or(true);
                if should_persist {
                    session.last_output_at = state::now_secs();
                    state::write_state(&session).ok();
                    last_output_persisted_at = Some(Instant::now());
                }
            }
            Ok(WorkerEvent::PtyEof) => {}
            Ok(WorkerEvent::CodexSessionId { id, path }) => {
                session.codex_session_id = Some(id);
                session.codex_rollout_path = Some(path.to_string_lossy().to_string());
                session.updated_at = state::now_secs();
                state::write_state(&session).ok();
            }
            Ok(WorkerEvent::ChildExit) => {
                session.status = STATUS_EXITED.to_string();
                session.updated_at = state::now_secs();
                state::write_state(&session).ok();
                fs::remove_file(&session.socket).ok();
                return Ok(());
            }
            Ok(WorkerEvent::ClientInput(client_id, bytes)) => {
                if is_current_client(&attached, client_id) {
                    pty_writer.write_all(&bytes).ok();
                    pty_writer.flush().ok();
                }
            }
            Ok(WorkerEvent::ClientResize(client_id, rows, cols)) => {
                if is_current_client(&attached, client_id) {
                    pair.master
                        .resize(PtySize {
                            rows: rows.max(1),
                            cols: cols.max(1),
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .ok();
                }
            }
            Ok(WorkerEvent::ClientDetach(client_id)) | Ok(WorkerEvent::ClientGone(client_id)) => {
                if is_current_client(&attached, client_id) {
                    attached = None;
                }
            }
            Ok(WorkerEvent::Stop) => {
                if stop_requested_at.is_none() {
                    session.status = STATUS_STOPPING.to_string();
                    session.updated_at = state::now_secs();
                    state::write_state(&session).ok();
                    signal_child(child_pid, libc::SIGTERM);
                    stop_requested_at = Some(Instant::now());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(started) = stop_requested_at {
            if started.elapsed() > Duration::from_secs(5) {
                signal_child(child_pid, libc::SIGKILL);
                stop_requested_at = Some(Instant::now());
            }
        }
    }

    Ok(())
}

fn accept_connections(
    listener: &UnixListener,
    tx: &Sender<WorkerEvent>,
    attached: &mut Option<(u64, UnixStream)>,
    next_client_id: &mut u64,
    master: &dyn MasterPty,
    session: &SessionState,
) -> Result<()> {
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) => return Err(err).context("accept client"),
        };

        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let line = match protocol::read_line(&mut stream) {
            Ok(line) => line,
            Err(_) => continue,
        };
        stream.set_read_timeout(None).ok();

        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("ATTACH") => {
                if attached.is_some() {
                    stream
                        .write_all(b"\r\n[rail] session is already attached elsewhere\r\n")
                        .ok();
                    stream.flush().ok();
                    continue;
                }

                let rows = parts
                    .next()
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap_or(INITIAL_ROWS)
                    .max(1);
                let cols = parts
                    .next()
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap_or(INITIAL_COLS)
                    .max(1);
                master
                    .resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .ok();

                let client_id = *next_client_id;
                *next_client_id += 1;
                send_log_tail(&mut stream, &session.id).ok();
                let reader = stream.try_clone().context("clone client stream")?;
                spawn_client_reader(reader, client_id, tx.clone());
                *attached = Some((client_id, stream));
            }
            Some("STOP") => {
                tx.send(WorkerEvent::Stop).ok();
            }
            _ => {}
        }
    }
}

fn send_log_tail(stream: &mut UnixStream, id: &str) -> Result<()> {
    let path = state::log_path(id);
    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES as u64);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Ok(());
    }

    let mut buf = [0_u8; 8192];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if stream.write_all(&buf[..n]).and_then(|_| stream.flush()).is_err() {
            break;
        }
    }
    Ok(())
}

fn spawn_pty_reader(mut reader: Box<dyn Read + Send>, tx: Sender<WorkerEvent>) {
    thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    tx.send(WorkerEvent::PtyEof).ok();
                    break;
                }
                Ok(n) => {
                    if tx.send(WorkerEvent::PtyOutput(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    tx.send(WorkerEvent::PtyEof).ok();
                    break;
                }
            }
        }
    });
}

fn spawn_client_reader(mut stream: UnixStream, client_id: u64, tx: Sender<WorkerEvent>) {
    thread::spawn(move || loop {
        match protocol::read_client_frame(&mut stream) {
            Ok(Some(ClientFrame::Input(bytes))) => {
                if tx.send(WorkerEvent::ClientInput(client_id, bytes)).is_err() {
                    break;
                }
            }
            Ok(Some(ClientFrame::Resize { rows, cols })) => {
                if tx
                    .send(WorkerEvent::ClientResize(client_id, rows, cols))
                    .is_err()
                {
                    break;
                }
            }
            Ok(Some(ClientFrame::Detach)) => {
                tx.send(WorkerEvent::ClientDetach(client_id)).ok();
                break;
            }
            Ok(None) => {
                tx.send(WorkerEvent::ClientGone(client_id)).ok();
                break;
            }
            Err(_) => {
                tx.send(WorkerEvent::ClientGone(client_id)).ok();
                break;
            }
        }
    });
}

fn is_current_client(attached: &Option<(u64, UnixStream)>, client_id: u64) -> bool {
    attached
        .as_ref()
        .map(|(current, _)| *current == client_id)
        .unwrap_or(false)
}

fn signal_child(pid: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(pid as libc::pid_t, signal);
        }
    }
}

// Correlates a freshly-spawned codex child with the rollout file it writes
// under ~/.codex/sessions/, so a later worker restart can `codex resume
// <id>` instead of losing the conversation. This format is undocumented and
// reverse-engineered, so it's deliberately best-effort: on any mismatch we
// just leave codex_session_id unset and fresh-spawn next time, same as today.
fn spawn_session_id_watcher(tx: Sender<WorkerEvent>, seen: HashSet<PathBuf>) {
    thread::spawn(move || {
        let root = state::codex_sessions_dir();
        // Generous giveup bound. Normally the file appears within a second or
        // two of codex startup; this only matters for slow cold starts. When
        // it is hit (e.g. the codex format differs and no match is ever
        // found) the cost is just a cheap directory walk every 300ms.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(300));
            let mut current = Vec::new();
            walk_jsonl(&root, 0, &mut current);
            let Some(new_path) = current.into_iter().find(|p| !seen.contains(p)) else {
                continue;
            };
            if let Some(session_id) = extract_codex_session_id(&new_path) {
                tx.send(WorkerEvent::CodexSessionId {
                    id: session_id,
                    path: new_path,
                })
                .ok();
            }
            return;
        }
    });
}

fn walk_jsonl(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, depth + 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn extract_codex_session_id(path: &Path) -> Option<String> {
    read_session_id_from_jsonl(path).or_else(|| extract_session_id_from_filename(path))
}

fn read_session_id_from_jsonl(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = io::BufReader::new(file);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).ok()?;
    let value: serde_json::Value = serde_json::from_str(first_line.trim()).ok()?;
    for key in ["session_id", "id"] {
        if let Some(id) = value.get(key).and_then(|v| v.as_str()) {
            return Some(id.to_string());
        }
        if let Some(id) = value
            .get("payload")
            .and_then(|p| p.get(key))
            .and_then(|v| v.as_str())
        {
            return Some(id.to_string());
        }
    }
    None
}

// Falls back to the rollout filename itself: `rollout-<timestamp>-<id>.jsonl`
// where <timestamp> looks like `2025-01-22T10-30-00`.
fn extract_session_id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("rollout-")?;
    let t_pos = rest.find('T')?;
    let after_t = &rest[t_pos + 1..];
    let mut parts = after_t.splitn(4, '-');
    parts.next()?; // hour
    parts.next()?; // minute
    parts.next()?; // second
    let id = parts.next()?;
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn mark_failed(id: &str, message: &str) {
    if let Ok(mut session) = state::read_state(id) {
        session.status = STATUS_FAILED.to_string();
        session.last_error = Some(message.to_string());
        session.updated_at = state::now_secs();
        state::write_state(&session).ok();
        fs::remove_file(&session.socket).ok();
    }
}

enum WorkerEvent {
    PtyOutput(Vec<u8>),
    PtyEof,
    CodexSessionId { id: String, path: PathBuf },
    ChildExit,
    ClientInput(u64, Vec<u8>),
    ClientResize(u64, u16, u16),
    ClientDetach(u64),
    ClientGone(u64),
    Stop,
}
