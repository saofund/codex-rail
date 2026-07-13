//! Per-session process guardian.
//!
//! The worker owns the PTY, but it is not a sufficient lifetime boundary: an
//! OOM/SIGKILL can kill the worker while Codex and detached MCP helpers remain.
//! The manager therefore launches this small subreaper as the worker's parent.
//! If the worker disappears for any reason, Linux reparents its remaining
//! generation here, where PPID ownership is available even when `/proc/environ`
//! is hidden on a shared host.

use crate::process_tree::{self, CleanupReport, RunGuard};
use crate::state::{self, STATUS_FAILED, STATUS_RUNNING, STATUS_STARTING, STATUS_STOPPING};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

pub fn run_guardian(id: &str) -> Result<()> {
    state::validate_session_id(id)?;
    unsafe {
        libc::setsid();
    }
    state::ensure_base_dirs()?;
    let _generation_lock = state::acquire_session_generation_lock(id)?;
    let subreaper = process_tree::enable_subreaper();

    let session = state::read_state(id)?;
    let mut child = Command::new(std::env::current_exe().context("current executable")?)
        .arg("--worker")
        .arg(id)
        .current_dir(Path::new(&session.cwd))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn guarded worker")?;
    let worker_pid = child.id();
    let mut generation = RunGuard::new(state::new_session_id(), subreaper);
    generation.track_root(Some(worker_pid));

    let status = loop {
        match child.try_wait().context("poll guarded worker")? {
            Some(status) => break status,
            None => {
                generation.refresh_if_due(Duration::from_millis(250));
                thread::sleep(Duration::from_millis(50));
            }
        }
    };
    generation.note_root_exited();
    let mut report = generation.terminate(Duration::from_secs(2), Duration::from_secs(2));
    if !subreaper
        && state::read_state_optional(id)?.is_some_and(|session| session.worker_token.is_some())
    {
        // The guardian token is intentionally not inherited by Codex (the
        // worker installs its own generation token). Without subreaper support,
        // an empty guardian lineage is not proof about daemonized helpers.
        report.verified = false;
        report.survivors = report.survivors.max(1);
    }
    record_guardian_finish(id, worker_pid, status, report)?;
    if report.is_clean() {
        Ok(())
    } else {
        anyhow::bail!(
            "guardian could not verify generation cleanup: {} survivor(s)",
            report.survivors
        )
    }
}

fn record_guardian_finish(
    id: &str,
    worker_pid: u32,
    status: ExitStatus,
    report: CleanupReport,
) -> Result<()> {
    let Some(mut session) = state::read_state_optional(id)? else {
        return Ok(());
    };
    // A different pid means state no longer describes our child. This should be
    // impossible while generation.lock is held, but fail closed if a broken
    // network flock violates the lease.
    if session
        .worker_pid
        .is_some_and(|current| current != worker_pid)
    {
        return Ok(());
    }

    let worker_left_runtime_state = matches!(
        session.status.as_str(),
        STATUS_STARTING | STATUS_RUNNING | STATUS_STOPPING
    );
    session.worker_pid = None;
    session.updated_at = state::now_secs();
    let worker_already_certified_cleanup =
        session.worker_token.is_none() && !worker_left_runtime_state;
    if worker_already_certified_cleanup {
        return state::write_state(&session);
    }
    if report.is_clean() {
        session.child_pid = None;
        session.worker_token = None;
        if worker_left_runtime_state {
            session.status = STATUS_FAILED.to_string();
            session.last_error = Some(match status.code() {
                Some(code) => format!(
                    "worker exited unexpectedly with status {code}; guardian cleaned its process generation"
                ),
                None => "worker was killed unexpectedly; guardian cleaned its process generation"
                    .to_string(),
            });
        }
    } else {
        session.status = STATUS_FAILED.to_string();
        session.last_error = Some(format!(
            "guardian process cleanup incomplete: {} survivor(s), census verified={}",
            report.survivors, report.verified
        ));
    }
    state::write_state(&session)
}
