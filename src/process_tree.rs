//! Generation-scoped process supervision for one Rail worker.
//!
//! A Codex process does not stay in one process group: MCP servers and helper
//! Codex instances may call `setsid(2)` or otherwise create their own groups.
//! Consequently, signalling only the PTY child PGID is not a complete stop.
//!
//! On Linux every process launched for a run inherits a unique environment
//! token.  We combine that marker with start-time-qualified `/proc` identities
//! and recursive PPID discovery.  Previously observed identities remain owned
//! by the run even after reparenting, while PID reuse is rejected by starttime.
//! Shutdown is TERM -> rediscover -> KILL -> rediscover/verify.
//!
//! Other Unix platforms deliberately use a narrower, fail-closed fallback: the
//! freshly spawned root process group is signalled, but unrelated processes are
//! never guessed from PIDs after the root exits.

use std::io;
use std::thread;
use std::time::{Duration, Instant};

pub const RUN_TOKEN_ENV: &str = "CODEX_RAIL_RUN_TOKEN";

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Best-effort cleanup result. `verified` means the platform could enumerate
/// the generation after the final signal; an empty survivor set on an
/// unsupported platform only proves that the owned root is gone.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleanupReport {
    pub term_signals: usize,
    pub kill_signals: usize,
    pub survivors: usize,
    pub verified: bool,
}

impl CleanupReport {
    pub fn is_clean(self) -> bool {
        #[cfg(target_os = "linux")]
        {
            // Linux promises a full generation census. If /proc enumeration
            // failed, an empty in-memory set is uncertainty, not proof.
            self.survivors == 0 && self.verified
        }

        #[cfg(not(target_os = "linux"))]
        {
            // Other Unix platforms intentionally promise only that the freshly
            // owned root process group is gone; detached setsid descendants are
            // not guessed from unqualified PIDs.
            self.survivors == 0 && self.verified
        }
    }
}

/// Ask Linux to reparent orphaned grandchildren to this worker instead of PID
/// 1. This lets the worker reap helpers after their intermediate MCP launcher
/// exits. Failure is non-fatal: token + lineage discovery still performs the
/// kill, but zombie reaping becomes the host init's responsibility.
pub fn enable_subreaper() -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == 0 }
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Clean a generation after its worker itself was SIGKILLed/OOM-killed. The
/// persisted worker token is sufficient on Linux even though no trustworthy
/// root PID remains. Callers must still serialize against worker startup (the
/// manager does so with its init + worker locks) before invoking this function.
///
/// On platforms without `/proc/<pid>/environ`, token-only cleanup cannot be
/// performed safely and returns an explicitly unclean, unverified report.
pub fn terminate_generation_by_token(
    token: &str,
    term_grace: Duration,
    kill_grace: Duration,
) -> CleanupReport {
    if !valid_token(token) {
        return CleanupReport {
            survivors: 1,
            verified: false,
            ..CleanupReport::default()
        };
    }

    #[cfg(target_os = "linux")]
    {
        let mut guard = RunGuard::new(token.to_string(), false);
        // There is intentionally no root identity: the full token census is the
        // ownership proof. Mark armed so terminate performs that first scan.
        guard.strict_token_census = true;
        guard.armed = true;
        guard.terminate(term_grace, kill_grace)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (term_grace, kill_grace);
        CleanupReport {
            survivors: 1,
            verified: false,
            ..CleanupReport::default()
        }
    }
}

fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Owns the process generation spawned with `token` for its entire lifetime.
/// If a worker leaves through any error path after spawn, Drop performs a short
/// bounded cleanup rather than abandoning Codex/MCP descendants.
pub struct RunGuard {
    token: String,
    armed: bool,
    root_pid: Option<libc::pid_t>,
    root_pgid: Option<libc::pid_t>,
    root_exited: bool,
    last_refresh: Instant,
    may_reap_adopted: bool,
    strict_token_census: bool,

    #[cfg(target_os = "linux")]
    root: Option<ProcRef>,
    #[cfg(target_os = "linux")]
    seen: std::collections::HashSet<ProcRef>,
    #[cfg(target_os = "linux")]
    live: std::collections::HashSet<ProcRef>,
}

impl RunGuard {
    pub fn new(token: String, may_reap_adopted: bool) -> Self {
        Self {
            token,
            armed: false,
            root_pid: None,
            root_pgid: None,
            root_exited: false,
            last_refresh: Instant::now(),
            may_reap_adopted,
            // Without subreaper support a daemonized descendant can leave the
            // root's PPID tree before the next refresh.  In that degraded mode
            // shutdown must fail closed unless the token census itself was
            // complete; silently claiming a clean generation would be worse.
            strict_token_census: !may_reap_adopted,
            #[cfg(target_os = "linux")]
            root: None,
            #[cfg(target_os = "linux")]
            seen: std::collections::HashSet::new(),
            #[cfg(target_os = "linux")]
            live: std::collections::HashSet::new(),
        }
    }

    /// Record the freshly spawned PTY child. Call immediately after spawn,
    /// before any other fallible worker setup.
    pub fn track_root(&mut self, pid: Option<u32>) {
        let Some(pid) = pid.and_then(checked_pid) else {
            return;
        };
        self.root_pid = Some(pid);
        let pgid = unsafe { libc::getpgid(pid) };
        if pgid > 1 {
            self.root_pgid = Some(pgid);
        }
        self.armed = true;

        #[cfg(target_os = "linux")]
        if let Some(info) = read_proc_info(pid) {
            self.root = Some(info.reference);
            self.seen.insert(info.reference);
            if info.is_live() {
                self.live.insert(info.reference);
            }
        }
    }

    /// The portable-pty waiter has reaped the direct child. On non-Linux this
    /// also prevents a later PID reuse from ever being signalled.
    pub fn note_root_exited(&mut self) {
        self.root_exited = true;
    }

    /// Periodically remember descendants while the run is healthy. This scan
    /// follows PPIDs only and is intentionally cheap; the full environment-token
    /// scan is reserved for shutdown, when independently reparented groups matter.
    pub fn refresh_if_due(&mut self, interval: Duration) {
        if self.last_refresh.elapsed() < interval {
            return;
        }
        self.last_refresh = Instant::now();
        #[cfg(target_os = "linux")]
        {
            let _ = self.refresh_linux(false);
        }
    }

    /// Stop every process owned by this run. TERM is given `term_grace`; any
    /// surviving/newly-discovered identities then receive KILL and are verified
    /// for up to `kill_grace`.
    pub fn terminate(&mut self, term_grace: Duration, kill_grace: Duration) -> CleanupReport {
        if !self.armed {
            return CleanupReport {
                verified: cfg!(target_os = "linux"),
                ..CleanupReport::default()
            };
        }

        #[cfg(target_os = "linux")]
        {
            self.terminate_linux(term_grace, kill_grace)
        }

        #[cfg(not(target_os = "linux"))]
        {
            self.terminate_process_group(term_grace, kill_grace)
        }
    }

    #[cfg(target_os = "linux")]
    fn refresh_linux(&mut self, include_tokens: bool) -> bool {
        let scan = match scan_proc(
            include_tokens,
            &self.token,
            &self.seen,
            self.strict_token_census,
            self.may_reap_adopted
                .then_some(std::process::id() as libc::pid_t),
        ) {
            Ok(snapshot) => snapshot,
            Err(_) => return false,
        };
        let snapshot = scan.processes;

        // If the child was so short-lived that track_root could not read stat,
        // recover its qualified identity from this snapshot when possible.
        if self.root.is_none() {
            if let Some(root_pid) = self.root_pid {
                if let Some(info) = snapshot.iter().find(|p| p.reference.pid == root_pid) {
                    self.root = Some(info.reference);
                    self.seen.insert(info.reference);
                }
            }
        }

        let selected = select_owned(
            &snapshot,
            &self.seen,
            self.may_reap_adopted
                .then_some(std::process::id() as libc::pid_t),
        );
        self.seen.extend(selected.iter().copied());
        self.live = selected;
        self.reap_adopted_children();
        !include_tokens || scan.token_census_complete
    }

    #[cfg(target_os = "linux")]
    fn terminate_linux(&mut self, term_grace: Duration, kill_grace: Duration) -> CleanupReport {
        let mut report = CleanupReport {
            verified: true,
            ..CleanupReport::default()
        };
        let mut term_sent = std::collections::HashSet::new();
        let term_deadline = Instant::now() + term_grace;

        loop {
            report.verified &= self.refresh_linux(true);
            if self.live.is_empty() {
                self.armed = false;
                return report;
            }
            for proc in self.signal_order() {
                if term_sent.insert(proc) && signal_if_same(proc, libc::SIGTERM) {
                    report.term_signals += 1;
                }
            }
            if Instant::now() >= term_deadline {
                break;
            }
            thread::sleep(
                POLL_INTERVAL.min(term_deadline.saturating_duration_since(Instant::now())),
            );
        }

        // A TERM handler can fork or daemonize. Rediscover on every KILL poll and
        // signal identities not present in the previous snapshot.
        let mut kill_sent = std::collections::HashSet::new();
        let kill_deadline = Instant::now() + kill_grace;
        loop {
            report.verified &= self.refresh_linux(true);
            if self.live.is_empty() {
                self.armed = false;
                return report;
            }
            for proc in self.signal_order() {
                if kill_sent.insert(proc) && signal_if_same(proc, libc::SIGKILL) {
                    report.kill_signals += 1;
                }
            }
            if Instant::now() >= kill_deadline {
                break;
            }
            thread::sleep(
                POLL_INTERVAL.min(kill_deadline.saturating_duration_since(Instant::now())),
            );
        }

        report.verified &= self.refresh_linux(true);
        report.survivors = self.live.len();
        if report.survivors == 0 {
            self.armed = false;
        }
        report
    }

    #[cfg(target_os = "linux")]
    fn signal_order(&self) -> Vec<ProcRef> {
        let mut members: Vec<_> = self.live.iter().copied().collect();
        // Signal the PTY root last, giving already-known helpers a chance to
        // handle TERM before their parent disappears/reparents them.
        members.sort_by_key(|proc| usize::from(Some(*proc) == self.root));
        members
    }

    #[cfg(target_os = "linux")]
    fn reap_adopted_children(&self) {
        if !self.may_reap_adopted {
            return;
        }
        for proc in &self.seen {
            if Some(*proc) == self.root {
                // portable-pty's waiter owns the direct child.
                continue;
            }
            let mut status = 0;
            unsafe {
                // If it has not been reparented to our subreaper, waitpid simply
                // returns ECHILD; never wait for or reap an unrelated process.
                libc::waitpid(proc.pid, &mut status, libc::WNOHANG);
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn terminate_process_group(
        &mut self,
        term_grace: Duration,
        kill_grace: Duration,
    ) -> CleanupReport {
        let mut report = CleanupReport {
            // No /proc token census exists here, so detached setsid descendants
            // cannot be verified without risking unrelated PIDs.
            verified: false,
            ..CleanupReport::default()
        };
        if self.root_exited {
            // The waiter already reaped the only PID that qualified this PGID.
            // An absent group proves there is nothing left; an extant group
            // cannot be signalled safely because the numeric PGID may since
            // have been reused. Report uncertainty instead of leaking silently
            // or risking an unrelated process group.
            if self
                .root_pgid
                .is_some_and(|pgid| !process_group_exists(pgid))
            {
                report.verified = true;
            } else {
                report.survivors = 1;
            }
            self.armed = false;
            return report;
        }
        let (Some(pid), Some(pgid)) = (self.root_pid, self.root_pgid) else {
            return report;
        };
        if process_group_still_owned(pid, pgid) && signal_group(pgid, libc::SIGTERM) {
            report.term_signals = 1;
        } else {
            report.survivors = 1;
            return report;
        }
        if wait_group_gone(pgid, term_grace) {
            report.verified = true;
            self.armed = false;
            return report;
        }
        if signal_group(pgid, libc::SIGKILL) {
            report.kill_signals = 1;
        }
        if wait_group_gone(pgid, kill_grace) {
            report.verified = true;
            self.armed = false;
        } else {
            report.survivors = 1;
        }
        report
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.armed {
            // Error/panic paths must not leak a run forever, but Drop is kept
            // bounded so worker teardown itself cannot wedge indefinitely.
            let _ = self.terminate(Duration::from_millis(200), Duration::from_millis(800));
        }
    }
}

fn checked_pid(pid: u32) -> Option<libc::pid_t> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    (pid > 1).then_some(pid)
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ProcRef {
    pid: libc::pid_t,
    start_time: u64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct ProcInfo {
    reference: ProcRef,
    ppid: libc::pid_t,
    state: u8,
    token_match: bool,
}

#[cfg(target_os = "linux")]
impl ProcInfo {
    fn is_live(&self) -> bool {
        !matches!(self.state, b'Z' | b'X' | b'x')
    }
}

#[cfg(target_os = "linux")]
fn parse_proc_stat(stat: &str) -> Option<ProcInfo> {
    let lp = stat.find('(')?;
    let rp = stat.rfind(')')?;
    if lp >= rp {
        return None;
    }
    let pid = stat[..lp].trim().parse().ok()?;
    let fields: Vec<&str> = stat[rp + 1..].split_whitespace().collect();
    let state = fields.first()?.as_bytes().first().copied()?;
    let ppid = fields.get(1)?.parse().ok()?;
    // starttime is proc(5) field 22; fields[0] here is field 3.
    let start_time = fields.get(19)?.parse().ok()?;
    Some(ProcInfo {
        reference: ProcRef { pid, start_time },
        ppid,
        state,
        token_match: false,
    })
}

#[cfg(target_os = "linux")]
fn read_proc_info(pid: libc::pid_t) -> Option<ProcInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_proc_stat(&stat)
}

#[cfg(target_os = "linux")]
struct ProcScan {
    processes: Vec<ProcInfo>,
    token_census_complete: bool,
}

#[cfg(target_os = "linux")]
fn scan_proc(
    check_token: bool,
    token: &str,
    previously_seen: &std::collections::HashSet<ProcRef>,
    strict_token_census: bool,
    adopted_parent: Option<libc::pid_t>,
) -> io::Result<ProcScan> {
    use std::os::unix::fs::MetadataExt;

    let mut out = Vec::new();
    let mut token_census_complete = true;
    let euid = unsafe { libc::geteuid() };
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if pid <= 1 || pid == std::process::id() as libc::pid_t {
            continue;
        }
        let Some(info) = read_proc_info(pid) else {
            continue;
        };
        out.push(info);
    }

    // PPID/starttime evidence is sufficient for already observed descendants.
    // Scope strict recovery to processes proven to share our PID namespace:
    // shared/container hosts can expose hundreds of same-UID processes from
    // sibling namespaces whose environ and namespace links are intentionally
    // unreadable. Requiring all of those would make every recovery permanently
    // unverifiable even though they cannot be this Rail generation.
    let definitely_owned = select_owned(&out, previously_seen, adopted_parent);
    let worker_pid = std::process::id() as libc::pid_t;
    for info in &mut out {
        if check_token && info.is_live() {
            let strict_scope = if strict_token_census {
                match same_pid_namespace(info.reference.pid) {
                    Ok(same) => same,
                    // A process exiting between snapshots cannot survive as a
                    // hidden generation member. Any other namespace-identity
                    // error is uncertainty and must prevent a verified result.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => false,
                    Err(_) => {
                        token_census_complete = false;
                        false
                    }
                }
            } else {
                false
            };
            let token_read_is_required = strict_scope
                || definitely_owned.contains(&info.reference)
                || info.ppid == worker_pid;
            let proc_path = format!("/proc/{}", info.reference.pid);
            let same_user = match std::fs::metadata(proc_path) {
                Ok(meta) => meta.uid() == euid,
                Err(err) if err.kind() == io::ErrorKind::NotFound => false,
                Err(_) => {
                    if token_read_is_required {
                        token_census_complete = false;
                    }
                    false
                }
            };
            if same_user {
                match environ_has_token(info.reference.pid, token) {
                    Ok(found) => info.token_match = found,
                    // Exiting between stat and environ is not an incomplete
                    // census; it cannot remain as a live survivor.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(_) if token_read_is_required => token_census_complete = false,
                    Err(_) => {}
                }
            }
        }
    }
    Ok(ProcScan {
        processes: out,
        token_census_complete,
    })
}

#[cfg(target_os = "linux")]
fn same_pid_namespace(pid: libc::pid_t) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let ours = std::fs::metadata("/proc/self/ns/pid")?;
    let theirs = std::fs::metadata(format!("/proc/{pid}/ns/pid"))?;
    Ok(ours.dev() == theirs.dev() && ours.ino() == theirs.ino())
}

#[cfg(target_os = "linux")]
fn environ_has_token(pid: libc::pid_t, token: &str) -> io::Result<bool> {
    use std::io::Read;

    const MAX_ENV_BYTES: u64 = 4 * 1024 * 1024;
    let file = std::fs::File::open(format!("/proc/{pid}/environ"))?;
    let mut bytes = Vec::new();
    file.take(MAX_ENV_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ENV_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process environment exceeds census bound",
        ));
    }
    let prefix = format!("{RUN_TOKEN_ENV}=");
    Ok(bytes.split(|b| *b == 0).any(|entry| {
        entry
            .strip_prefix(prefix.as_bytes())
            .map(|value| value == token.as_bytes())
            .unwrap_or(false)
    }))
}

#[cfg(target_os = "linux")]
fn select_owned(
    snapshot: &[ProcInfo],
    previously_seen: &std::collections::HashSet<ProcRef>,
    adopted_parent: Option<libc::pid_t>,
) -> std::collections::HashSet<ProcRef> {
    let mut selected: std::collections::HashSet<ProcRef> = snapshot
        .iter()
        .filter(|info| {
            info.is_live()
                && (info.token_match
                    || previously_seen.contains(&info.reference)
                    // When this worker successfully became a child subreaper,
                    // an orphan directly reparented to it is generation-owned
                    // even if PR_SET_DUMPABLE made /proc/environ unreadable.
                    || adopted_parent == Some(info.ppid))
        })
        .map(|info| info.reference)
        .collect();

    loop {
        let parent_pids: std::collections::HashSet<libc::pid_t> =
            selected.iter().map(|proc| proc.pid).collect();
        let before = selected.len();
        for info in snapshot {
            if info.is_live() && parent_pids.contains(&info.ppid) {
                selected.insert(info.reference);
            }
        }
        if selected.len() == before {
            break;
        }
    }
    selected
}

#[cfg(target_os = "linux")]
fn signal_if_same(proc: ProcRef, signal: libc::c_int) -> bool {
    let Some(current) = read_proc_info(proc.pid) else {
        return false;
    };
    if current.reference != proc || !current.is_live() {
        return false;
    }
    unsafe { libc::kill(proc.pid, signal) == 0 }
}

#[cfg(not(target_os = "linux"))]
fn process_group_still_owned(pid: libc::pid_t, pgid: libc::pid_t) -> bool {
    if pid <= 1 || pgid <= 1 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 && libc::getpgid(pid) == pgid }
}

#[cfg(not(target_os = "linux"))]
fn signal_group(pgid: libc::pid_t, signal: libc::c_int) -> bool {
    if pgid <= 1 || pgid == unsafe { libc::getpgrp() } {
        return false;
    }
    unsafe { libc::kill(-pgid, signal) == 0 }
}

#[cfg(not(target_os = "linux"))]
fn process_group_exists(pgid: libc::pid_t) -> bool {
    if pgid <= 1 {
        return false;
    }
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(target_os = "linux"))]
fn wait_group_gone(pgid: libc::pid_t, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_group_exists(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{
        parse_proc_stat, select_owned, terminate_generation_by_token, valid_token, CleanupReport,
        ProcInfo, ProcRef,
    };
    use std::collections::HashSet;
    use std::time::Duration;

    fn info(pid: i32, ppid: i32, start_time: u64, token_match: bool) -> ProcInfo {
        ProcInfo {
            reference: ProcRef { pid, start_time },
            ppid,
            state: b'S',
            token_match,
        }
    }

    #[test]
    fn parses_starttime_after_a_tricky_comm() {
        // fields after ')' are 3..22; the command may itself contain spaces and ')'.
        let middle = std::iter::repeat("0")
            .take(17)
            .collect::<Vec<_>>()
            .join(" ");
        let stat = format!("42 (mcp helper) odd)) S 7 {middle} 98765");
        let parsed = parse_proc_stat(&stat).expect("valid synthetic stat");
        assert_eq!(
            parsed.reference,
            ProcRef {
                pid: 42,
                start_time: 98765
            }
        );
        assert_eq!(parsed.ppid, 7);
    }

    #[test]
    fn token_and_ppid_find_reparented_and_independent_groups() {
        // PID 20 represents an MCP that called setsid: process-group changes do
        // not matter because ownership follows the token/lineage, not PGID.
        // PID 30 was already seen before reparenting to PID 1 and scrubbed its env.
        let old_reparented = ProcRef {
            pid: 30,
            start_time: 300,
        };
        let previously_seen = HashSet::from([old_reparented]);
        let snapshot = vec![
            info(10, 2, 100, true),
            info(20, 10, 200, false),
            info(21, 20, 210, false),
            info(30, 1, 300, false),
            info(40, 2, 400, false),
        ];
        let selected = select_owned(&snapshot, &previously_seen, None);
        assert_eq!(selected.len(), 4);
        assert!(selected.contains(&ProcRef {
            pid: 10,
            start_time: 100
        }));
        assert!(selected.contains(&ProcRef {
            pid: 20,
            start_time: 200
        }));
        assert!(selected.contains(&ProcRef {
            pid: 21,
            start_time: 210
        }));
        assert!(selected.contains(&old_reparented));
        assert!(!selected.contains(&ProcRef {
            pid: 40,
            start_time: 400
        }));
    }

    #[test]
    fn pid_reuse_does_not_inherit_ownership() {
        let previously_seen = HashSet::from([ProcRef {
            pid: 50,
            start_time: 500,
        }]);
        let snapshot = vec![info(50, 1, 999, false)];
        assert!(select_owned(&snapshot, &previously_seen, None).is_empty());
    }

    #[test]
    fn subreaper_adopted_children_are_owned_without_readable_environment() {
        let snapshot = vec![info(70, 7, 700, false), info(71, 70, 710, false)];
        let selected = select_owned(&snapshot, &HashSet::new(), Some(7));
        assert_eq!(selected.len(), 2);
        assert!(selected.contains(&ProcRef {
            pid: 70,
            start_time: 700
        }));
        assert!(selected.contains(&ProcRef {
            pid: 71,
            start_time: 710
        }));
    }

    #[test]
    fn failed_linux_census_is_not_clean() {
        let report = CleanupReport {
            survivors: 0,
            verified: false,
            ..CleanupReport::default()
        };
        assert!(!report.is_clean());
    }

    #[test]
    fn token_only_cleanup_rejects_an_unscoped_token() {
        assert!(!valid_token(""));
        assert!(!valid_token("contains/slash"));
        let report = terminate_generation_by_token("", Duration::ZERO, Duration::ZERO);
        assert!(!report.is_clean());
        assert!(!report.verified);
    }
}
