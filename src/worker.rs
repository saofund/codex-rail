use crate::protocol::{self, ClientFrame};
use crate::state::{
    self, SessionState, STATUS_EXITED, STATUS_FAILED, STATUS_RUNNING, STATUS_STOPPING,
};
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
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

    let mut attached: Option<(u64, UnixStream)> = None;
    let mut next_client_id = 1_u64;
    let mut stop_requested_at: Option<Instant> = None;

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
            }
            Ok(WorkerEvent::PtyEof) => {}
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
    ChildExit,
    ClientInput(u64, Vec<u8>),
    ClientResize(u64, u16, u16),
    ClientDetach(u64),
    ClientGone(u64),
    Stop,
}
