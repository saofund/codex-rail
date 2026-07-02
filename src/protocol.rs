use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

const FRAME_INPUT: u8 = 0;
const FRAME_RESIZE: u8 = 1;
const FRAME_DETACH: u8 = 2;

// Generous bound for a single paste/keystroke burst. Caps allocation from a
// peer-controlled length prefix so a malformed frame can't make the worker
// try to allocate gigabytes.
const MAX_INPUT_FRAME: usize = 1024 * 1024;

pub enum ClientFrame {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Detach,
}

pub fn read_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::with_capacity(64);
    let mut one = [0_u8; 1];

    loop {
        let n = stream.read(&mut one)?;
        if n == 0 {
            break;
        }
        if one[0] == b'\n' {
            break;
        }
        bytes.push(one[0]);
        if bytes.len() > 16 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control line too long",
            ));
        }
    }

    Ok(String::from_utf8_lossy(&bytes).trim_end().to_string())
}

pub fn write_input_frame(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "input frame too large"))?;
    stream.write_all(&[FRAME_INPUT])?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(bytes)?;
    stream.flush()
}

pub fn write_resize_frame(stream: &mut UnixStream, rows: u16, cols: u16) -> io::Result<()> {
    stream.write_all(&[FRAME_RESIZE])?;
    stream.write_all(&rows.to_be_bytes())?;
    stream.write_all(&cols.to_be_bytes())?;
    stream.flush()
}

pub fn write_detach_frame(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(&[FRAME_DETACH])?;
    stream.flush()
}

pub fn read_client_frame(stream: &mut UnixStream) -> io::Result<Option<ClientFrame>> {
    let mut tag = [0_u8; 1];
    if !read_exact_or_eof(stream, &mut tag)? {
        return Ok(None);
    }

    match tag[0] {
        FRAME_INPUT => {
            let mut len_bytes = [0_u8; 4];
            if !read_exact_or_eof(stream, &mut len_bytes)? {
                return Ok(None);
            }
            let len = u32::from_be_bytes(len_bytes) as usize;
            if len > MAX_INPUT_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "input frame too large",
                ));
            }
            let mut bytes = vec![0_u8; len];
            if !read_exact_or_eof(stream, &mut bytes)? {
                return Ok(None);
            }
            Ok(Some(ClientFrame::Input(bytes)))
        }
        FRAME_RESIZE => {
            let mut rows = [0_u8; 2];
            let mut cols = [0_u8; 2];
            if !read_exact_or_eof(stream, &mut rows)? {
                return Ok(None);
            }
            if !read_exact_or_eof(stream, &mut cols)? {
                return Ok(None);
            }
            Ok(Some(ClientFrame::Resize {
                rows: u16::from_be_bytes(rows),
                cols: u16::from_be_bytes(cols),
            }))
        }
        FRAME_DETACH => Ok(Some(ClientFrame::Detach)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown client frame: {other}"),
        )),
    }
}

fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}
