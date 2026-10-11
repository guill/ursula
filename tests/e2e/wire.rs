//! NDJSON connections with deadlines, event readers and `audio` request builders.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Value};

pub const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Samples per `audio` request: 3200 bytes of PCM.
pub const CHUNK: usize = 1600;

pub struct Conn {
    pub r: BufReader<UnixStream>,
    pub w: UnixStream,
}

impl Conn {
    pub fn new(sock: UnixStream) -> Self {
        sock.set_read_timeout(Some(IO_TIMEOUT))
            .expect("read timeout");
        sock.set_write_timeout(Some(IO_TIMEOUT))
            .expect("write timeout");
        let w = sock.try_clone().expect("clone socket");
        Self {
            r: BufReader::new(sock),
            w,
        }
    }

    pub fn send(&mut self, req: &Value) {
        self.send_raw(&format!("{req}\n"));
    }

    pub fn send_raw(&mut self, text: &str) {
        self.w.write_all(text.as_bytes()).expect("write request");
    }

    pub fn expect(&mut self, name: &str, timeout: Duration) -> Value {
        expect_event(&mut self.r, name, timeout)
    }

    /// Sends `req` (a `start`) and returns the session id from `started`.
    pub fn start(&mut self, req: &Value) -> String {
        self.send(req);
        let started = self.expect("started", IO_TIMEOUT);
        started["session"].as_str().expect("session id").to_owned()
    }

    /// Reads until EOF or a read error; panics if neither happens within `timeout`.
    pub fn read_to_close(&mut self, timeout: Duration) -> Vec<Value> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            match read_line_by(&mut self.r, deadline) {
                Ok(Some(line)) => seen.push(parse(&line)),
                Ok(None) => return seen,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    panic!(
                        "connection still open after {timeout:?}; {} lines",
                        seen.len()
                    )
                }
                Err(_) => return seen,
            }
        }
    }
}

/// One line (without `\n`) before `deadline`; `Ok(None)` at EOF. The read timeout is
/// re-armed with the remaining time before every read, so trickling bytes cannot extend it.
pub fn read_line_by(
    r: &mut BufReader<UnixStream>,
    deadline: Instant,
) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        r.get_ref().set_read_timeout(Some(left))?;
        let buf = match r.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::ErrorKind::TimedOut.into())
            }
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            return Ok(None);
        }
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&buf[..i]);
            r.consume(i + 1);
            return Ok(Some(line));
        }
        let n = buf.len();
        line.extend_from_slice(buf);
        r.consume(n);
    }
}

fn parse(line: &[u8]) -> Value {
    serde_json::from_slice(line).unwrap_or_else(|_| String::from_utf8_lossy(line).into())
}

/// Reads events until one named `name`; panics with everything seen at EOF or at the
/// absolute deadline `now + timeout`.
pub fn expect_event(r: &mut BufReader<UnixStream>, name: &str, timeout: Duration) -> Value {
    expect_event_any(r, &[name], timeout)
}

/// [`expect_event`] for the first event named any of `names`.
pub fn expect_event_any(r: &mut BufReader<UnixStream>, names: &[&str], timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        match read_line_by(r, deadline) {
            Ok(Some(line)) => {
                let event = parse(&line);
                if names.iter().any(|n| event["event"] == *n) {
                    return event;
                }
                seen.push(event);
            }
            Ok(None) => panic!("EOF while waiting for {names:?}; seen {seen:?}"),
            Err(e) => panic!("no {names:?} within {timeout:?} ({e}); seen {seen:?}"),
        }
    }
}

/// NDJSON `audio` lines for `pcm`, [`CHUNK`] samples each.
pub fn audio_lines(pcm: &[i16]) -> String {
    pcm.chunks(CHUNK)
        .map(|c| {
            let bytes: Vec<u8> = c.iter().flat_map(|s| s.to_le_bytes()).collect();
            format!(
                "{}\n",
                json!({"op": "audio", "data": STANDARD.encode(bytes)})
            )
        })
        .collect()
}

pub fn push_start() -> Value {
    json!({"op": "start", "source": "push"})
}
