use crate::distill;
use crate::process_tree::{self, CleanupReport, RunGuard, RUN_TOKEN_ENV};
use crate::protocol::{self, ClientFrame};
use crate::state::{
    self, SessionState, STATUS_EXITED, STATUS_FAILED, STATUS_RUNNING, STATUS_STOPPING,
};
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;
const TAIL_BYTES: usize = 96 * 1024;
const MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;
const RETAIN_LOG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ACCEPTS_PER_TICK: usize = 8;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_millis(500);
const BRACKETED_PASTE_ENABLE: &[u8] = b"\x1b[?2004h";
const BRACKETED_PASTE_DISABLE: &[u8] = b"\x1b[?2004l";
const SCREEN_CLEAR: &[u8] = b"\x1b[2J";
const SCREEN_CLEAR_SCROLLBACK: &[u8] = b"\x1b[3J";
const ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";
const ALT_SCREEN_LEAVE: &[u8] = b"\x1b[?1049l";
const ALT_SCREEN_1047_ENTER: &[u8] = b"\x1b[?1047h";
const ALT_SCREEN_1047_LEAVE: &[u8] = b"\x1b[?1047l";
const CODEX_COMPOSER_GLYPH: &[u8] = "›".as_bytes();
const COMPOSER_QUIET_TIME: Duration = Duration::from_millis(150);
const CONTROL_QUEUE_CAPACITY: usize = 32;
const PTY_QUEUE_CAPACITY: usize = 64;

fn initial_prompt_ready_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("CODEX_RAIL_PROMPT_READY_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(60),
    )
}

#[derive(Default)]
struct TuiReadiness {
    pending: Vec<u8>,
    paste_enabled: bool,
    composer_seen: bool,
    last_output_at: Option<Instant>,
}

impl TuiReadiness {
    fn observe(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // The composer is usable only after the whole screen has settled, not
        // merely 150ms after the first prompt-looking glyph.  A loading status
        // or dialog can redraw after `›`; every later PTY byte therefore
        // restarts the quiet window while keeping the candidate latched.
        self.last_output_at = Some(Instant::now());
        self.pending.extend_from_slice(bytes);
        let patterns = [
            BRACKETED_PASTE_ENABLE,
            BRACKETED_PASTE_DISABLE,
            SCREEN_CLEAR,
            SCREEN_CLEAR_SCROLLBACK,
            ALT_SCREEN_ENTER,
            ALT_SCREEN_LEAVE,
            ALT_SCREEN_1047_ENTER,
            ALT_SCREEN_1047_LEAVE,
            CODEX_COMPOSER_GLYPH,
        ];
        let mut cursor = 0;
        while cursor < self.pending.len() {
            let rest = &self.pending[cursor..];
            if rest.starts_with(BRACKETED_PASTE_ENABLE) {
                self.paste_enabled = true;
                self.composer_seen = false;
                cursor += BRACKETED_PASTE_ENABLE.len();
            } else if rest.starts_with(BRACKETED_PASTE_DISABLE) {
                self.paste_enabled = false;
                self.composer_seen = false;
                cursor += BRACKETED_PASTE_DISABLE.len();
            } else if let Some(reset) = [
                SCREEN_CLEAR,
                SCREEN_CLEAR_SCROLLBACK,
                ALT_SCREEN_ENTER,
                ALT_SCREEN_LEAVE,
                ALT_SCREEN_1047_ENTER,
                ALT_SCREEN_1047_LEAVE,
            ]
            .into_iter()
            .find(|sequence| rest.starts_with(sequence))
            {
                self.composer_seen = false;
                cursor += reset.len();
            } else if rest.starts_with(CODEX_COMPOSER_GLYPH) {
                if self.paste_enabled {
                    self.composer_seen = true;
                }
                cursor += CODEX_COMPOSER_GLYPH.len();
            } else if patterns.iter().any(|pattern| pattern.starts_with(rest)) {
                break; // a control sequence split across PTY reads
            } else {
                cursor += 1;
            }
        }
        self.pending.drain(..cursor);
        if self.pending.len() > 32 {
            let keep_from = self.pending.len() - 32;
            self.pending.drain(..keep_from);
        }
    }

    fn ready(&self) -> bool {
        self.paste_enabled
            && self.composer_seen
            && self
                .last_output_at
                .is_some_and(|at| at.elapsed() >= COMPOSER_QUIET_TIME)
    }
}

pub fn run_worker(id: &str) -> Result<()> {
    state::validate_session_id(id)?;
    unsafe {
        libc::setsid();
    }

    state::ensure_base_dirs()?;
    let subreaper_enabled = process_tree::enable_subreaper();
    // Hold one kernel-backed lock for the worker's entire lifetime. Two manager
    // processes can race to resume the same stopped/imported session; without
    // this guard they would start two `codex resume` writers and the later worker
    // would unlink the first worker's live socket.
    let Some(_worker_lock) = state::try_acquire_worker_lock(id)? else {
        // Another worker won the resume race. This is a normal idempotent
        // outcome: do not mark the winning worker's session failed.
        return Ok(());
    };
    let Some(_socket_lock) = state::try_acquire_socket_lock(id)? else {
        return Ok(());
    };

    match run_worker_inner(id, subreaper_enabled) {
        Ok(()) => Ok(()),
        Err(err) => {
            mark_failed_if_owner(id, &format!("{err:#}"));
            Err(err)
        }
    }
}

fn run_worker_inner(id: &str, subreaper_enabled: bool) -> Result<()> {
    let mut session = state::read_state(id)?;
    let _distill_run_lock = if session.distill_version.is_some() {
        Some(distill::worker_run_lock().context("claim distillation lifetime lock")?)
    } else {
        None
    };
    if session.codex_session_id.is_none() {
        if let Some((codex_id, rollout)) = state::claimed_rollout_for_session(id) {
            session.codex_session_id = Some(codex_id);
            session.codex_rollout_path = Some(rollout);
            persist_state(&session).context("recover atomically claimed Codex rollout")?;
        }
    }
    if session.initial_prompt_injecting {
        let report = session
            .worker_token
            .as_deref()
            .map(|token| {
                process_tree::terminate_generation_by_token(
                    token,
                    Duration::from_secs(2),
                    Duration::from_secs(2),
                )
            })
            .unwrap_or(CleanupReport {
                survivors: 1,
                verified: false,
                ..CleanupReport::default()
            });
        session.status = STATUS_FAILED.to_string();
        session.worker_pid = None;
        if report.is_clean() {
            session.child_pid = None;
            session.worker_token = None;
        }
        session.initial_prompt = None;
        session.initial_prompt_injecting = false;
        session.updated_at = state::now_secs();
        session.last_error = Some(if report.is_clean() {
            "initial prompt delivery was interrupted and may have been submitted; the abandoned process generation was cleaned, so inspect the transcript before resending"
                .to_string()
        } else {
            format!(
                "initial prompt delivery was interrupted and cleanup could not verify the abandoned generation ({} survivor(s)); refusing to start another Codex",
                report.survivors
            )
        });
        persist_state(&session)?;
        if report.is_clean() {
            return Ok(());
        }
        anyhow::bail!("interrupted initial prompt generation cleanup was not verified");
    }
    if session.codex_session_id.is_some() && session.initial_prompt.is_some() {
        let Some(rollout) = session.codex_rollout_path.as_deref() else {
            session.status = STATUS_FAILED.to_string();
            session.last_error = Some(
                "pending initial prompt belongs to a resumed Codex session but its rollout path is unknown; refusing an unverified resend"
                    .to_string(),
            );
            session.updated_at = state::now_secs();
            persist_state(&session)?;
            return Ok(());
        };
        let rollout_matches_session = state::rollout_head(Path::new(rollout))
            .map(|(_, rollout_id)| Some(rollout_id) == session.codex_session_id)
            .unwrap_or(false);
        if !rollout_matches_session {
            session.status = STATUS_FAILED.to_string();
            session.last_error = Some(
                "pending initial prompt rollout does not match the persisted Codex session id; refusing an unverified resend"
                    .to_string(),
            );
            session.updated_at = state::now_secs();
            persist_state(&session)?;
            return Ok(());
        }
        match state::rollout_has_genuine_user_message(Path::new(rollout)) {
            Ok(true) => {
                // A human or older worker already submitted a genuine turn.
                // Clear the stale pending copy rather than duplicate it.
                session.initial_prompt = None;
                persist_state(&session)?;
            }
            Ok(false) => {
                // Definitely never submitted: keep it pending and inject only
                // after the resumed TUI's real composer is ready.
            }
            Err(err) => {
                session.status = STATUS_FAILED.to_string();
                session.last_error = Some(format!(
                    "cannot safely decide whether the pending initial prompt was already submitted: {err:#}"
                ));
                session.updated_at = state::now_secs();
                persist_state(&session)?;
                return Ok(());
            }
        }
    }
    let socket_path = state::socket_path(id);
    if socket_path.exists() {
        // Compatibility with workers started by an older rail build (which do
        // not hold worker.lock): never unlink a socket that still has a listener.
        match UnixStream::connect(&socket_path) {
            Ok(_) => return Ok(()),
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::ENOENT) | Some(libc::ECONNREFUSED) | Some(libc::ENOTSOCK)
                ) => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("probe existing socket {}", socket_path.display()))
            }
        }
    }

    if socket_path.exists() {
        let meta = fs::symlink_metadata(&socket_path)
            .with_context(|| format!("inspect stale socket {}", socket_path.display()))?;
        if !meta.file_type().is_socket() {
            anyhow::bail!(
                "refuse to remove non-socket at worker path {}",
                socket_path.display()
            );
        }
        fs::remove_file(&socket_path)
            .with_context(|| format!("remove stale socket {}", socket_path.display()))?;
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(err)
            if err.raw_os_error() == Some(libc::EADDRINUSE)
                && UnixStream::connect(&socket_path).is_ok() =>
        {
            // The socket bind is the final atomic ownership boundary. We have
            // not claimed state yet, so losing this race cannot clobber the
            // listener's pid/token even if a data-dir flock was unreliable.
            return Ok(());
        }
        Err(err) => return Err(err).with_context(|| format!("bind {}", socket_path.display())),
    };
    state::restrict_file_to_owner(&socket_path)?;
    let socket_identity = fs::symlink_metadata(&socket_path)
        .map(|m| (m.dev(), m.ino()))
        .with_context(|| format!("identify socket {}", socket_path.display()))?;
    listener
        .set_nonblocking(true)
        .context("set listener nonblocking")?;

    // Only the canonical socket winner may claim runtime state.
    session.socket = socket_path.to_string_lossy().to_string();
    session.worker_pid = Some(process::id());
    session.child_pid = None;
    session.worker_lock_protocol = true;
    let run_token = state::new_session_id();
    session.worker_token = Some(run_token.clone());
    session.status = crate::state::STATUS_STARTING.to_string();
    session.exit_code = None;
    session.last_error = None;
    session.updated_at = state::now_secs();
    persist_state(&session)?;

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
    // Every process in this worker generation inherits a unique ownership
    // marker. MCP servers routinely create their own process groups/sessions,
    // so PGID alone is not a complete cleanup boundary.
    cmd.env(RUN_TOKEN_ENV, &run_token);
    // Extra flags (e.g. a distill session's `-C <dir> -s workspace-write` plus a
    // trust override) go BEFORE the prompt/resume args so codex parses them as
    // options. Empty for ordinary sessions.
    for a in &session.codex_args {
        cmd.arg(a);
    }
    // Prompts are deliberately NOT argv: `/proc/<pid>/cmdline` is commonly
    // world-readable and distill/pilot prompts can contain private transcript
    // context. Fresh-session prompts are injected through the owned PTY once
    // the TUI has started; resume already has its conversation and never replays
    // a leftover first prompt.
    let mut initial_prompt = session.initial_prompt.clone();
    let is_resume = match &session.codex_session_id {
        Some(codex_session_id) => {
            cmd.arg("resume");
            cmd.arg(codex_session_id);
            true
        }
        None => false,
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

    let (tx, rx) = mpsc::sync_channel(CONTROL_QUEUE_CAPACITY);
    let (pty_tx, pty_rx) = mpsc::sync_channel(PTY_QUEUE_CAPACITY);
    let mut run_guard = RunGuard::new(run_token, subreaper_enabled);
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawn {}", session.codex))?;
    let child_pid = child.process_id();
    run_guard.track_root(child_pid);
    drop(pair.slave);

    // Install the waiter immediately after spawn. Any later fallible setup is
    // protected by run_guard's Drop cleanup, while this thread reaps the PTY
    // root and reports its exit to the worker loop.
    {
        let tx = tx.clone();
        thread::spawn(move || {
            let outcome = match child.wait() {
                Ok(status) => ChildOutcome {
                    exit_code: Some(i32::try_from(status.exit_code()).unwrap_or(1)),
                    success: status.success(),
                    wait_error: None,
                },
                Err(err) => ChildOutcome {
                    exit_code: None,
                    success: false,
                    wait_error: Some(err.to_string()),
                },
            };
            tx.send(WorkerEvent::ChildExit(outcome)).ok();
        });
    }

    session.child_pid = child_pid;
    session.status = STATUS_RUNNING.to_string();
    session.updated_at = state::now_secs();
    persist_state(&session)?;

    let pty_reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let mut pty_writer = Some(pair.master.take_writer().context("take pty writer")?);
    let log_path = state::log_path(id);
    let mut log = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&log_path)
        .context("open output log")?;
    if !log.metadata()?.is_file() {
        anyhow::bail!("output log is not a regular file: {}", log_path.display());
    }
    log.set_permissions(fs::Permissions::from_mode(0o600))?;
    let mut log_bytes = log.metadata().map(|meta| meta.len()).unwrap_or(0);

    spawn_pty_reader(pty_reader, pty_tx, tx.clone());
    if let Some(before) = codex_sessions_before {
        spawn_session_id_watcher(
            tx.clone(),
            before,
            session.cwd.clone(),
            session.id.clone(),
            child_pid,
            initial_prompt
                .as_deref()
                .map(protocol::sanitize_submission_text),
        );
    }

    let mut attached: Option<AttachedClient> = None;
    let mut next_client_id = 1_u64;
    let mut last_output_persisted_at: Option<Instant> = None;
    let prompt_wait_started = Instant::now();
    let prompt_ready_timeout = initial_prompt_ready_timeout();
    let mut tui_readiness = TuiReadiness::default();
    let mut last_distill_completion_check = Instant::now();
    let mut rollout_lifecycle = RolloutLifecycle::default();
    // After a headless injection is acknowledged, do not accept another one
    // until the rollout proves Codex started a successor turn.  This closes the
    // small DELIVERED -> task_started window where a second client could
    // otherwise submit a duplicate reply against the same waiting marker.
    let mut injected_before_started_turn: Option<u64> = None;
    const OUTPUT_PERSIST_INTERVAL: Duration = Duration::from_secs(2);

    loop {
        // Remember descendants while their PPID links are intact. Shutdown also
        // performs a full environment-token census, so helpers that later setsid
        // or reparent remain in this generation's ownership set.
        run_guard.refresh_if_due(Duration::from_secs(2));
        let lifecycle_waiting = session
            .codex_rollout_path
            .as_deref()
            .and_then(|path| rollout_lifecycle.scan(Path::new(path)));
        if injected_before_started_turn
            .is_some_and(|baseline| rollout_lifecycle.started_turns > baseline)
        {
            injected_before_started_turn = None;
        }
        let injection_ready = lifecycle_waiting == Some(true)
            && injected_before_started_turn.is_none()
            && !session.initial_prompt_injecting
            && session.initial_prompt.is_none();
        if accept_connections(
            &listener,
            &tx,
            &mut attached,
            &mut next_client_id,
            &*pair.master,
            &session,
            injection_ready,
        )? {
            return stop_run(&mut session, &mut run_guard, socket_identity);
        }

        let event = match rx.try_recv() {
            Ok(event) => Some(event),
            Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {
                match pty_rx.recv_timeout(Duration::from_millis(80)) {
                    Ok(bytes) => Some(WorkerEvent::PtyOutput(bytes)),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => None,
                }
            }
        };
        match event {
            Some(WorkerEvent::PtyOutput(bytes)) => {
                tui_readiness.observe(&bytes);
                if log_bytes.saturating_add(bytes.len() as u64) > MAX_LOG_BYTES {
                    if compact_output_log(&mut log, RETAIN_LOG_BYTES).is_ok() {
                        log_bytes = log.metadata().map(|meta| meta.len()).unwrap_or(0);
                    }
                }
                if log.write_all(&bytes).is_ok() {
                    log_bytes = log_bytes.saturating_add(bytes.len() as u64);
                }
                log.flush().ok();
                if let Some(client) = attached.as_mut().filter(|client| !client.headless) {
                    if client
                        .stream
                        .write_all(&bytes)
                        .and_then(|_| client.stream.flush())
                        .is_err()
                    {
                        disconnect_attached(&mut attached);
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
                    persist_state(&session).ok();
                    last_output_persisted_at = Some(Instant::now());
                }
            }
            Some(WorkerEvent::PtyEof) => {}
            Some(WorkerEvent::CodexSessionId { id, path }) => {
                session.codex_session_id = Some(id);
                session.codex_rollout_path = Some(path.to_string_lossy().to_string());
                session.updated_at = state::now_secs();
                persist_state(&session).context("persist claimed Codex rollout identity")?;
            }
            Some(WorkerEvent::ChildExit(outcome)) => {
                run_guard.note_root_exited();
                let report = run_guard.terminate(Duration::from_secs(1), Duration::from_secs(2));
                return finish_run(
                    &mut session,
                    report,
                    FinishCause::Natural(outcome),
                    socket_identity,
                );
            }
            Some(WorkerEvent::ClientInput(client_id, bytes)) => {
                if is_current_client(&attached, client_id) {
                    let headless = attached
                        .as_ref()
                        .is_some_and(|client| client.id == client_id && client.headless);
                    let delivered = pty_writer
                        .as_mut()
                        .map(|writer| writer.write_all(&bytes).and_then(|_| writer.flush()))
                        .transpose()
                        .and_then(|result| {
                            result.ok_or_else(|| {
                                io::Error::new(io::ErrorKind::WouldBlock, "PTY writer is busy")
                            })
                        })
                        .is_ok();
                    if headless && delivered {
                        injected_before_started_turn = Some(rollout_lifecycle.started_turns);
                    }
                    if let Some(client) = attached.as_mut().filter(|c| c.id == client_id) {
                        if client.headless {
                            let response: &[u8] = if delivered {
                                b"DELIVERED\n"
                            } else {
                                b"FAILED\n"
                            };
                            if client
                                .stream
                                .write_all(response)
                                .and_then(|_| client.stream.flush())
                                .is_err()
                            {
                                disconnect_attached(&mut attached);
                            }
                        } else if !delivered {
                            disconnect_attached(&mut attached);
                        }
                    }
                    // INJECT is a one-frame protocol.  Closing immediately
                    // after the acknowledgement prevents a malicious or buggy
                    // peer from streaming a second logical submission through
                    // the already-authorized headless slot.
                    if headless {
                        disconnect_attached(&mut attached);
                    }
                }
            }
            Some(WorkerEvent::ClientResize(client_id, rows, cols)) => {
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
            Some(WorkerEvent::ClientDetach(client_id))
            | Some(WorkerEvent::ClientGone(client_id)) => {
                if is_current_client(&attached, client_id) {
                    attached = None;
                }
            }
            Some(WorkerEvent::InitialPromptWritten { writer, error }) => {
                pty_writer = Some(writer);
                if let Some(error) = error {
                    anyhow::bail!("write initial prompt: {error}");
                }
                session.initial_prompt = None;
                session.initial_prompt_injecting = false;
                persist_state(&session).context("record initial prompt delivery")?;
            }
            None => {}
        }

        if state::take_worker_stop_request(&session).unwrap_or(false) {
            return stop_run(&mut session, &mut run_guard, socket_identity);
        }

        if tui_readiness.ready() && initial_prompt.is_some() {
            session.initial_prompt_injecting = true;
            persist_state(&session).context("record initial prompt delivery intent")?;
            let prompt = initial_prompt.take().expect("checked above");
            let mut writer = pty_writer.take().context("initial prompt writer is busy")?;
            let tx = tx.clone();
            thread::spawn(move || {
                let error = write_initial_prompt(&mut *writer, &prompt)
                    .err()
                    .map(|err| err.to_string());
                tx.send(WorkerEvent::InitialPromptWritten { writer, error })
                    .ok();
            });
        }

        if initial_prompt.is_some() && prompt_wait_started.elapsed() >= prompt_ready_timeout {
            anyhow::bail!(
                "Codex composer did not become ready for the private initial prompt within {}s; attach to resolve any startup dialog, then retry",
                prompt_ready_timeout.as_secs()
            );
        }

        if session.distill_version.is_some()
            && last_distill_completion_check.elapsed() >= Duration::from_secs(1)
        {
            last_distill_completion_check = Instant::now();
            match distill_run_completion(&session, lifecycle_waiting == Some(true)) {
                DistillRunCompletion::Pending => {}
                DistillRunCompletion::Complete => {
                    let Some(corpus) = session.distill_corpus_rel.clone() else {
                        return fail_run(
                            &mut session,
                            &mut run_guard,
                            socket_identity,
                            "validated distillation has no persisted private corpus path",
                        );
                    };
                    if let Err(err) = distill::cleanup_run_corpus(&corpus) {
                        return fail_run(
                            &mut session,
                            &mut run_guard,
                            socket_identity,
                            &format!(
                                "distillation output validated but private corpus cleanup failed: {err:#}"
                            ),
                        );
                    }
                    if let Some(version) = session.distill_version {
                        distill::mark_output_validated(version)
                            .context("persist distillation validation marker")?;
                    }
                    session.distill_validated = true;
                    persist_state(&session).context("record validated distillation output")?;
                    return stop_run(&mut session, &mut run_guard, socket_identity);
                }
                DistillRunCompletion::Invalid(reason) => {
                    return fail_run(
                        &mut session,
                        &mut run_guard,
                        socket_identity,
                        &format!("distillation output failed coverage validation: {reason}"),
                    );
                }
            }
        }
    }

    Ok(())
}

fn write_initial_prompt(writer: &mut dyn Write, prompt: &str) -> io::Result<()> {
    let wire = protocol::bracketed_submission(prompt);
    writer.write_all(&wire)?;
    writer.flush()
}

enum DistillRunCompletion {
    Pending,
    Complete,
    Invalid(String),
}

fn distill_run_completion(session: &SessionState, rollout_waiting: bool) -> DistillRunCompletion {
    let (Some(version), Some(_rollout)) = (
        session.distill_version,
        session.codex_rollout_path.as_deref(),
    ) else {
        return DistillRunCompletion::Pending;
    };
    if !rollout_waiting {
        return DistillRunCompletion::Pending;
    }
    let Some(expected_turns) = session.distill_expected_user_turns else {
        return DistillRunCompletion::Invalid(
            "legacy distillation has no persisted coverage contract; output requires manual review"
                .to_string(),
        );
    };
    let output = state::distill_dir().join(format!("style-v{version:03}.md"));
    match distill::validate_output_contract(
        &output,
        &session.distill_expected_markers,
        expected_turns,
    ) {
        Ok(()) => DistillRunCompletion::Complete,
        Err(err) => DistillRunCompletion::Invalid(format!("{err:#}")),
    }
}

#[derive(Default)]
struct RolloutLifecycle {
    path: PathBuf,
    offset: u64,
    last_waiting: Option<bool>,
    partial_record: Vec<u8>,
    discarding_oversized_record: bool,
    caught_up: bool,
    started_turns: u64,
}

impl RolloutLifecycle {
    /// Incrementally consume the exact rollout from byte zero.  Returning None
    /// means fail-closed/busy: I/O failed, no lifecycle marker exists, or the
    /// bounded scanner has not caught up yet.  Unlike a fixed tail read, this
    /// latches the last marker across arbitrarily large unrelated records.
    fn scan(&mut self, path: &Path) -> Option<bool> {
        if self.path != path {
            self.path = path.to_path_buf();
            self.reset();
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() {
            return None;
        }
        let len = meta.len();
        if len < self.offset {
            self.reset();
        }
        if len > self.offset {
            file.seek(SeekFrom::Start(self.offset)).ok()?;
            const MAX_RECORD_BYTES: usize = 1024 * 1024;
            const MAX_SCAN_BYTES_PER_TICK: usize = 8 * 1024 * 1024;
            let mut scanned = 0usize;
            let mut buf = [0_u8; 64 * 1024];
            while scanned < MAX_SCAN_BYTES_PER_TICK {
                let want = buf.len().min(MAX_SCAN_BYTES_PER_TICK - scanned);
                let read = file.read(&mut buf[..want]).ok()?;
                if read == 0 {
                    break;
                }
                scanned += read;
                self.offset = self.offset.saturating_add(read as u64);
                for byte in &buf[..read] {
                    if self.discarding_oversized_record {
                        if *byte == b'\n' {
                            self.discarding_oversized_record = false;
                        }
                        continue;
                    }
                    if *byte == b'\n' {
                        self.observe_record();
                        self.partial_record.clear();
                    } else if self.partial_record.len() < MAX_RECORD_BYTES {
                        self.partial_record.push(*byte);
                    } else {
                        self.partial_record.clear();
                        self.discarding_oversized_record = true;
                    }
                }
            }
        }
        self.caught_up = self.offset >= len
            && self.partial_record.is_empty()
            && !self.discarding_oversized_record;
        if self.caught_up {
            self.last_waiting
        } else {
            None
        }
    }

    fn reset(&mut self) {
        self.offset = 0;
        self.last_waiting = None;
        self.partial_record.clear();
        self.discarding_oversized_record = false;
        self.caught_up = false;
        self.started_turns = 0;
    }

    fn observe_record(&mut self) {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&self.partial_record) else {
            return;
        };
        match value
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(|kind| kind.as_str())
        {
            Some("task_started") => {
                self.last_waiting = Some(false);
                self.started_turns = self.started_turns.saturating_add(1);
            }
            Some("task_complete" | "turn_aborted" | "thread_rolled_back") => {
                self.last_waiting = Some(true);
            }
            _ => {}
        }
    }
}

fn stop_run(
    session: &mut SessionState,
    run_guard: &mut RunGuard,
    socket_identity: (u64, u64),
) -> Result<()> {
    session.status = STATUS_STOPPING.to_string();
    session.updated_at = state::now_secs();
    persist_state(session).ok();
    let report = run_guard.terminate(Duration::from_secs(5), Duration::from_secs(2));
    finish_run(session, report, FinishCause::Stopped, socket_identity)
}

fn fail_run(
    session: &mut SessionState,
    run_guard: &mut RunGuard,
    socket_identity: (u64, u64),
    reason: &str,
) -> Result<()> {
    session.status = STATUS_STOPPING.to_string();
    session.updated_at = state::now_secs();
    persist_state(session).ok();
    let report = run_guard.terminate(Duration::from_secs(5), Duration::from_secs(2));
    session.status = STATUS_FAILED.to_string();
    session.updated_at = state::now_secs();
    session.worker_pid = None;
    session.initial_prompt = None;
    session.initial_prompt_injecting = false;
    session.last_error = Some(if report.is_clean() {
        reason.to_string()
    } else {
        format!(
            "{reason}; process cleanup incomplete: {} survivor(s), census verified={}",
            report.survivors, report.verified
        )
    });
    if report.is_clean() {
        session.child_pid = None;
        session.worker_token = None;
    }
    let persisted = persist_state(session);
    remove_socket_if_same(Path::new(&session.socket), socket_identity);
    persisted?;
    if report.is_clean() {
        Ok(())
    } else {
        anyhow::bail!("{reason}; generation cleanup was not verified")
    }
}

fn finish_run(
    session: &mut SessionState,
    report: CleanupReport,
    cause: FinishCause,
    socket_identity: (u64, u64),
) -> Result<()> {
    let disposition = classify_finish(report, cause);
    let prompt_delivery_uncertain = session.initial_prompt_injecting;
    session.updated_at = state::now_secs();
    session.status = if prompt_delivery_uncertain {
        STATUS_FAILED.to_string()
    } else {
        disposition.status.to_string()
    };
    session.exit_code = disposition.exit_code;
    session.last_error = if prompt_delivery_uncertain {
        let warning = "initial prompt delivery was interrupted and may have been submitted";
        Some(match disposition.last_error {
            Some(error) => format!("{warning}; {error}"),
            None => warning.to_string(),
        })
    } else {
        disposition.last_error
    };
    session.worker_pid = None;
    session.initial_prompt = if prompt_delivery_uncertain {
        None
    } else {
        session.initial_prompt.take()
    };
    session.initial_prompt_injecting = false;
    if report.is_clean() {
        session.child_pid = None;
        session.worker_token = None;
    }
    let persisted = persist_state(session);
    remove_socket_if_same(Path::new(&session.socket), socket_identity);
    persisted?;
    if !disposition.cleanup_incomplete {
        Ok(())
    } else {
        anyhow::bail!(
            "process cleanup incomplete after {} TERM and {} KILL signals: {} survivor(s)",
            report.term_signals,
            report.kill_signals,
            report.survivors
        )
    }
}

#[derive(Debug)]
struct ChildOutcome {
    exit_code: Option<i32>,
    success: bool,
    wait_error: Option<String>,
}

enum FinishCause {
    Natural(ChildOutcome),
    Stopped,
}

struct FinishDisposition {
    status: &'static str,
    exit_code: Option<i32>,
    last_error: Option<String>,
    cleanup_incomplete: bool,
}

fn classify_finish(report: CleanupReport, cause: FinishCause) -> FinishDisposition {
    let cleanup_incomplete = !report.is_clean();
    let (exit_code, child_error) = match cause {
        FinishCause::Stopped => (None, None),
        FinishCause::Natural(outcome) if outcome.success => (outcome.exit_code, None),
        FinishCause::Natural(outcome) => {
            let error = outcome
                .wait_error
                .unwrap_or_else(|| match outcome.exit_code {
                    Some(code) => format!("codex exited with status {code}"),
                    None => "codex exited unsuccessfully without a status code".to_string(),
                });
            (outcome.exit_code, Some(error))
        }
    };

    let cleanup_error = if report.survivors > 0 {
        Some(format!(
            "process cleanup left {} generation-owned process{} alive",
            report.survivors,
            if report.survivors == 1 { "" } else { "es" }
        ))
    } else if cleanup_incomplete {
        Some("process cleanup could not verify the owned generation boundary".to_string())
    } else {
        None
    };
    let last_error = match (child_error, cleanup_error) {
        (Some(child), Some(cleanup)) => Some(format!("{child}; {cleanup}")),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    };

    FinishDisposition {
        status: if last_error.is_some() {
            STATUS_FAILED
        } else {
            STATUS_EXITED
        },
        exit_code,
        last_error,
        cleanup_incomplete,
    }
}

fn remove_socket_if_same(path: &Path, expected: (u64, u64)) {
    let same = fs::symlink_metadata(path)
        .map(|m| m.file_type().is_socket() && (m.dev(), m.ino()) == expected)
        .unwrap_or(false);
    if same {
        fs::remove_file(path).ok();
    }
}

fn accept_connections(
    listener: &UnixListener,
    tx: &SyncSender<WorkerEvent>,
    attached: &mut Option<AttachedClient>,
    next_client_id: &mut u64,
    master: &dyn MasterPty,
    session: &SessionState,
    injection_ready: bool,
) -> Result<bool> {
    for _ in 0..MAX_ACCEPTS_PER_TICK {
        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(err) => return Err(err).context("accept client"),
        };

        stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT)).ok();
        stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT)).ok();
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
                if send_log_tail(&mut stream, &session.id).is_err() {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                let reader = stream.try_clone().context("clone client stream")?;
                spawn_client_reader(reader, client_id, tx.clone());
                *attached = Some(AttachedClient {
                    id: client_id,
                    stream,
                    headless: false,
                });
            }
            Some("INJECT") => {
                if attached.is_some() || !injection_ready {
                    stream.write_all(b"BUSY\n").ok();
                    stream.flush().ok();
                    continue;
                }
                if stream
                    .write_all(b"READY\n")
                    .and_then(|_| stream.flush())
                    .is_err()
                {
                    continue;
                }
                let client_id = *next_client_id;
                *next_client_id += 1;
                // A headless peer must send its one input frame promptly.  A
                // client that disappears after READY cannot reserve the attach
                // slot forever.
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let reader = stream.try_clone().context("clone injector stream")?;
                spawn_client_reader(reader, client_id, tx.clone());
                *attached = Some(AttachedClient {
                    id: client_id,
                    stream,
                    headless: true,
                });
            }
            Some("STOP") => {
                stream.write_all(b"STOPPING\n").ok();
                stream.flush().ok();
                return Ok(true);
            }
            _ => {}
        }
    }
    Ok(false)
}

struct AttachedClient {
    id: u64,
    stream: UnixStream,
    headless: bool,
}

fn disconnect_attached(attached: &mut Option<AttachedClient>) {
    if let Some(client) = attached.take() {
        let _ = client.stream.shutdown(Shutdown::Both);
    }
}

fn compact_output_log(log: &mut fs::File, retain: u64) -> io::Result<()> {
    let len = log.metadata()?.len();
    if len <= retain {
        return Ok(());
    }
    let start = len.saturating_sub(retain);
    let mut reader = log.try_clone()?;
    reader.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::with_capacity(retain as usize);
    reader.read_to_end(&mut tail)?;
    let tail = clean_tail_start(&tail, start > 0);
    log.set_len(0)?;
    log.write_all(b"\x1b[0m\r\n[rail] earlier output truncated\r\n")?;
    log.write_all(tail)?;
    log.flush()
}

fn send_log_tail(stream: &mut UnixStream, id: &str) -> Result<()> {
    let path = state::log_path(id);
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };
    if !file.metadata()?.is_file() {
        anyhow::bail!("output log is not a regular file: {}", path.display());
    }
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES as u64);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Ok(());
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Ok(());
    }
    // Snap the replay to a clean line boundary. Seeking to len-TAIL_BYTES lands on
    // an arbitrary byte — usually mid-line, mid-escape-sequence, or mid-UTF8 char —
    // and replaying from there garbles the first row(s) on attach. When we started
    // mid-file, drop that partial first line.
    let tail = clean_tail_start(&buf, start > 0);
    let _ = stream.write_all(tail).and_then(|_| stream.flush());
    Ok(())
}

// The slice of a log tail to replay on attach: from just after the first newline
// when the read began mid-file (so the replay starts on a clean line boundary,
// never mid-escape / mid-UTF8), else the whole buffer. A mid-file tail with no
// newline at all (one giant line) is replayed whole rather than dropped.
fn clean_tail_start(buf: &[u8], started_midfile: bool) -> &[u8] {
    if !started_midfile {
        return buf;
    }
    match buf.iter().position(|&c| c == b'\n') {
        Some(nl) => &buf[nl + 1..],
        None => buf,
    }
}

fn spawn_pty_reader(
    mut reader: Box<dyn Read + Send>,
    output_tx: SyncSender<Vec<u8>>,
    control_tx: SyncSender<WorkerEvent>,
) {
    thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    control_tx.send(WorkerEvent::PtyEof).ok();
                    break;
                }
                Ok(n) => {
                    if output_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    control_tx.send(WorkerEvent::PtyEof).ok();
                    break;
                }
            }
        }
    });
}

fn spawn_client_reader(mut stream: UnixStream, client_id: u64, tx: SyncSender<WorkerEvent>) {
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

fn is_current_client(attached: &Option<AttachedClient>, client_id: u64) -> bool {
    attached
        .as_ref()
        .map(|current| current.id == client_id)
        .unwrap_or(false)
}

// Correlates a freshly-spawned codex child with the rollout file it writes
// under ~/.codex/sessions/, so a later worker restart can `codex resume
// <id>` instead of losing the conversation. This format is undocumented and
// reverse-engineered, so it's deliberately best-effort: on any mismatch we
// just leave codex_session_id unset and fresh-spawn next time, same as today.
fn spawn_session_id_watcher(
    tx: SyncSender<WorkerEvent>,
    seen: HashSet<PathBuf>,
    cwd: String,
    rail_id: String,
    child_pid: Option<u32>,
    expected_first_prompt: Option<String>,
) {
    thread::spawn(move || {
        let root = state::codex_sessions_dir();
        let mut seen = seen;
        // Prefer the file descriptor actually held by this Codex process. Cwd
        // and creation time are not identities: two Rail sessions in one repo
        // routinely create rollouts within the same second.
        let deadline = Instant::now() + Duration::from_secs(6 * 3600);
        let started = Instant::now();
        let mut last_tree_scan = Instant::now() - Duration::from_secs(1);
        let mut fallback_candidate: Option<(PathBuf, Instant)> = None;
        while Instant::now() < deadline {
            if let Some(path) = child_pid.and_then(|pid| rollout_open_by_process(pid, &root)) {
                if claim_and_report_rollout(&tx, &rail_id, path) {
                    return;
                }
            }

            // Portable fallback for a prompted session: require its exact first
            // genuine user message and an atomic global path claim. Blank
            // sessions never guess; on Linux their open fd is the proof.
            if last_tree_scan.elapsed() >= Duration::from_secs(1) {
                if let Some(expected) = expected_first_prompt.as_deref() {
                    last_tree_scan = Instant::now();
                    let mut current = Vec::new();
                    walk_jsonl(&root, 0, &mut current);
                    let mut matching = Vec::new();
                    for path in current {
                        if seen.contains(&path) {
                            continue;
                        }
                        let Some((rollout_cwd, _)) = state::rollout_head(&path) else {
                            continue;
                        };
                        if rollout_cwd != cwd {
                            seen.insert(path);
                            continue;
                        }
                        match state::rollout_first_user_message(&path) {
                            Some(first) if first == expected => {
                                matching.push(path);
                            }
                            Some(_) => {
                                seen.insert(path);
                            }
                            None => {} // header exists, first turn not flushed yet
                        }
                    }
                    if matching.len() == 1 {
                        let path = matching.pop().expect("checked one candidate");
                        let stable = fallback_candidate.as_ref().is_some_and(|(prior, at)| {
                            prior == &path && at.elapsed() >= Duration::from_secs(1)
                        });
                        if stable && claim_and_report_rollout(&tx, &rail_id, path.clone()) {
                            return;
                        }
                        if !fallback_candidate
                            .as_ref()
                            .is_some_and(|(prior, _)| prior == &path)
                        {
                            fallback_candidate = Some((path, Instant::now()));
                        }
                    } else {
                        // Two identical first prompts are not distinguishable by
                        // content. Wait for the process-fd proof; never swap them.
                        fallback_candidate = None;
                    }
                }
            }

            let delay = if started.elapsed() < Duration::from_secs(30) {
                Duration::from_millis(200)
            } else if started.elapsed() < Duration::from_secs(10 * 60) {
                Duration::from_secs(2)
            } else {
                Duration::from_secs(15)
            };
            thread::sleep(delay);
        }
    });
}

fn claim_and_report_rollout(tx: &SyncSender<WorkerEvent>, rail_id: &str, path: PathBuf) -> bool {
    if !state::try_claim_rollout(rail_id, &path).unwrap_or(false) {
        return false;
    }
    let Some(session_id) = extract_codex_session_id(&path) else {
        return false;
    };
    tx.send(WorkerEvent::CodexSessionId {
        id: session_id,
        path,
    })
    .is_ok()
}

#[cfg(target_os = "linux")]
fn rollout_open_by_process(pid: u32, root: &Path) -> Option<PathBuf> {
    let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
    for entry in fs::read_dir(fd_dir).ok()?.flatten() {
        let Ok(path) = fs::read_link(entry.path()) else {
            continue;
        };
        if path.starts_with(root)
            && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            && state::rollout_head(&path).is_some()
        {
            return Some(path);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn rollout_open_by_process(pid: u32, root: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args(["-a", "-p", &pid.to_string(), "-Fn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(name) = line.strip_prefix('n') else {
            continue;
        };
        let path = PathBuf::from(name);
        if path.starts_with(root)
            && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            && state::rollout_head(&path).is_some()
        {
            return Some(path);
        }
    }
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rollout_open_by_process(_pid: u32, _root: &Path) -> Option<PathBuf> {
    None
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

// Persist the worker's runtime view. The user-facing label (title + pin) lives
// in a separate label.json written only by the manager and is authoritative on
// load, so the worker never needs to preserve — and cannot clobber — the title.
// This is what makes a rename stick even against an old or duplicate worker.
fn persist_state(session: &SessionState) -> Result<()> {
    state::write_state(session)
}

fn mark_failed_if_owner(id: &str, message: &str) {
    if let Ok(mut session) = state::read_state(id) {
        if session.worker_pid != Some(process::id()) {
            return;
        }
        session.status = STATUS_FAILED.to_string();
        session.last_error = Some(message.to_string());
        session.updated_at = state::now_secs();
        state::write_state(&session).ok();
        // Do not unlink here: after an EADDRINUSE race the pathname may belong
        // to a legacy live worker. A refused stale socket is safely removed by
        // the next owner during its probe phase.
    }
}

enum WorkerEvent {
    PtyOutput(Vec<u8>),
    PtyEof,
    CodexSessionId {
        id: String,
        path: PathBuf,
    },
    ChildExit(ChildOutcome),
    ClientInput(u64, Vec<u8>),
    ClientResize(u64, u16, u16),
    ClientDetach(u64),
    ClientGone(u64),
    InitialPromptWritten {
        writer: Box<dyn Write + Send>,
        error: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        classify_finish, clean_tail_start, compact_output_log, write_initial_prompt, ChildOutcome,
        FinishCause, RolloutLifecycle, TuiReadiness,
    };
    use crate::process_tree::CleanupReport;
    use crate::state::{STATUS_EXITED, STATUS_FAILED};
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::Write;

    #[test]
    fn tail_replay_starts_on_a_clean_line() {
        // mid-file: drop the partial first line (through the first '\n')
        assert_eq!(
            clean_tail_start(b"tial line\x1b[m\nclean\nrest", true),
            b"clean\nrest"
        );
        // whole-file read: keep everything, including the first line
        assert_eq!(clean_tail_start(b"first\nsecond", false), b"first\nsecond");
        // mid-file but one giant line with no newline: replay whole, don't drop all
        assert_eq!(
            clean_tail_start(b"no newline at all", true),
            b"no newline at all"
        );
        // mid-file, newline only at the very end: nothing clean to show
        assert_eq!(clean_tail_start(b"partial\n", true), b"");
    }

    #[test]
    fn natural_nonzero_exit_is_failed_but_an_active_stop_is_not() {
        let clean = CleanupReport {
            verified: true,
            ..CleanupReport::default()
        };
        let failed = classify_finish(
            clean,
            FinishCause::Natural(ChildOutcome {
                exit_code: Some(7),
                success: false,
                wait_error: None,
            }),
        );
        assert_eq!(failed.status, STATUS_FAILED);
        assert_eq!(failed.exit_code, Some(7));
        assert!(failed
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("status 7")));

        let stopped = classify_finish(clean, FinishCause::Stopped);
        assert_eq!(stopped.status, STATUS_EXITED);
        assert_eq!(stopped.exit_code, None);
        assert_eq!(stopped.last_error, None);
    }

    #[test]
    fn initial_prompt_is_bracketed_and_cannot_synthesize_terminal_controls() {
        let mut out = Vec::new();
        write_initial_prompt(&mut out, "first\nsecond\u{1b}[201~evil\u{202e}").unwrap();
        assert_eq!(out, b"\x1b[200~first\nsecond[201~evil\x1b[201~\r");
    }

    #[test]
    fn prompt_readiness_requires_paste_mode_and_codex_composer_across_chunks() {
        let mut readiness = TuiReadiness::default();
        readiness.observe(b"splash\x1b[?20");
        readiness.observe(b"04h loading");
        readiness.observe(b"still starting");
        readiness.observe(" draw › composer".as_bytes());
        std::thread::sleep(super::COMPOSER_QUIET_TIME + std::time::Duration::from_millis(20));
        assert!(readiness.ready());
        readiness.observe(b"\x1b[?2004l");
        assert!(!readiness.ready());

        let mut dialog = TuiReadiness::default();
        dialog.observe("old › menu\x1b[?2004h Trust this folder? [y/N]".as_bytes());
        std::thread::sleep(super::COMPOSER_QUIET_TIME + std::time::Duration::from_millis(20));
        assert!(!dialog.ready());
    }

    #[test]
    fn output_log_compaction_keeps_a_bounded_clean_tail() {
        let path = std::env::temp_dir().join(format!(
            "rail-output-log-{}-{}",
            std::process::id(),
            crate::state::now_millis()
        ));
        fs::write(
            &path,
            format!("old-prefix\n{}\nfinal-line\n", "x".repeat(8192)),
        )
        .unwrap();
        let mut log = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        compact_output_log(&mut log, 1024).unwrap();
        log.write_all(b"after\n").unwrap();
        drop(log);
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() < 1200);
        assert!(bytes.starts_with(b"\x1b[0m\r\n[rail] earlier output truncated\r\n"));
        assert!(bytes.ends_with(b"final-line\nafter\n"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn lifecycle_waiting_survives_an_oversized_unrelated_tail() {
        let path = std::env::temp_dir().join(format!(
            "rail-worker-lifecycle-{}-{}",
            std::process::id(),
            crate::state::now_millis()
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n")
            .unwrap();
        let noise = vec![b'x'; 1024 * 1024];
        for _ in 0..9 {
            file.write_all(&noise).unwrap();
        }
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();

        let mut lifecycle = RolloutLifecycle::default();
        assert_eq!(lifecycle.scan(&path), None); // bounded first tick, fail closed
        assert_eq!(lifecycle.scan(&path), Some(true));

        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n")
            .unwrap();
        file.flush().unwrap();
        assert_eq!(lifecycle.scan(&path), Some(false));
        assert_eq!(lifecycle.started_turns, 1);
        drop(file);
        let _ = fs::remove_file(path);
    }
}
