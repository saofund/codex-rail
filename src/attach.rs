use crate::protocol;
use crate::state::SessionState;
use anyhow::{Context, Result};
use crossterm::terminal;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const DETACH_BYTE: u8 = 0x1a;

pub fn attach_session(session: &SessionState) -> Result<()> {
    let mut stream = UnixStream::connect(&session.socket)
        .with_context(|| format!("connect {}", session.socket))?;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    writeln!(stream, "ATTACH {rows} {cols}")?;
    stream.flush()?;

    terminal::enable_raw_mode().context("enable raw mode for attach")?;
    let raw_guard = RawModeGuard;
    let result = attach_loop(stream, rows, cols);
    drop(raw_guard);
    result
}

fn attach_loop(mut stream: UnixStream, rows: u16, cols: u16) -> Result<()> {
    let stdin = io::stdin();
    let stdin_fd = stdin.as_raw_fd();
    let socket_fd = stream.as_raw_fd();
    let mut stdout = io::stdout();
    let mut input_buf = [0_u8; 8192];
    let mut output_buf = [0_u8; 8192];
    let mut last_size = (rows, cols);
    let mut last_resize_check = Instant::now();

    loop {
        let mut fds = [
            libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: socket_fd,
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
        ];

        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 150) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("poll attach fds");
        }

        if fds[0].revents & libc::POLLIN != 0 {
            let n = unsafe {
                libc::read(
                    stdin_fd,
                    input_buf.as_mut_ptr() as *mut libc::c_void,
                    input_buf.len(),
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err).context("read terminal input");
                }
            } else if n == 0 {
                protocol::write_detach_frame(&mut stream).ok();
                return Ok(());
            } else {
                let bytes = &input_buf[..n as usize];
                if let Some(pos) = bytes.iter().position(|b| *b == DETACH_BYTE) {
                    protocol::write_input_frame(&mut stream, &bytes[..pos])?;
                    protocol::write_detach_frame(&mut stream).ok();
                    return Ok(());
                }
                protocol::write_input_frame(&mut stream, bytes)?;
            }
        }

        if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(());
        }

        if fds[1].revents & libc::POLLIN != 0 {
            let n = stream
                .read(&mut output_buf)
                .context("read session output")?;
            if n == 0 {
                return Ok(());
            }
            stdout
                .write_all(&output_buf[..n])
                .context("write session output")?;
            stdout.flush().ok();
        }

        if last_resize_check.elapsed() >= Duration::from_millis(250) {
            last_resize_check = Instant::now();
            if let Ok((new_cols, new_rows)) = terminal::size() {
                let next = (new_rows, new_cols);
                if next != last_size {
                    protocol::write_resize_frame(&mut stream, new_rows, new_cols).ok();
                    last_size = next;
                }
            }
        }
    }
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        terminal::disable_raw_mode().ok();
    }
}
