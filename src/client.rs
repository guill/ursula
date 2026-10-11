//! CLI client commands: thin NDJSON clients of the daemon socket.
//!
//! Every command returns `Err(line)` for its stderr diagnostic (exit 1);
//! stdout (`out`) only ever receives transcript text or daemon output.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;

use serde_json::Value;

use crate::protocol::{Envelope, Request, Source};

const CLOSED: &str = "error: the daemon closed the connection early";

/// What one daemon event means to a session command.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Stream-mode segment: print it and keep reading.
    Segment(String),
    /// Session finished: print the text when `Some`, exit 0.
    Done(Option<String>),
    /// Session failed: this stderr line, exit 1.
    Fail(String),
}

/// Maps a session event to its outcome; `None` = not relevant, keep reading.
fn outcome(event: &Value, stream: bool) -> Option<Outcome> {
    let text = || event["text"].as_str().unwrap_or_default().to_owned();
    match event["event"].as_str()? {
        "segment" if stream => Some(Outcome::Segment(text())),
        "done" => Some(Outcome::Done((!stream).then(text))),
        "cancelled" => Some(Outcome::Fail("cancelled".to_owned())),
        "error" => Some(Outcome::Fail(error_line(event))),
        _ => None,
    }
}

fn error_line(event: &Value) -> String {
    let field = |key: &str| event[key].as_str().unwrap_or_default();
    format!("error: {}: {}", field("code"), field("message"))
}

fn connect(socket: &Path) -> Result<UnixStream, String> {
    UnixStream::connect(socket).map_err(|e| {
        format!(
            "error: cannot connect to {}: {e} (is 'ursula serve' running?)",
            socket.display()
        )
    })
}

fn send(mut stream: &UnixStream, req: Request) -> Result<(), String> {
    let mut line =
        serde_json::to_string(&Envelope { id: None, req }).map_err(|e| format!("error: {e}"))?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("error: cannot send request: {e}"))
}

/// Reply lines until EOF (a read error counts as EOF).
fn lines(stream: &UnixStream) -> impl Iterator<Item = String> + '_ {
    BufReader::new(stream).lines().map_while(Result::ok)
}

fn events(stream: &UnixStream) -> impl Iterator<Item = Value> + '_ {
    lines(stream).filter_map(|line| serde_json::from_str(&line).ok())
}

fn print(out: &mut dyn Write, text: &str) -> Result<(), String> {
    writeln!(out, "{text}")
        .and_then(|()| out.flush())
        .map_err(|e| format!("error: cannot write output: {e}"))
}

/// First ^C asks the daemon to stop (the result still prints); the second
/// exits 130. A failed install is only a warning: the session still works.
fn stop_on_ctrlc(stream: &UnixStream) {
    let mut stop = stream.try_clone().ok();
    let installed = ctrlc::set_handler(move || match stop.take() {
        Some(conn) => {
            // A failed write means the daemon is gone; the event loop sees EOF.
            let _ = send(&conn, Request::Stop { session: None });
        }
        None => std::process::exit(130),
    });
    if let Err(e) = installed {
        eprintln!("warning: Ctrl-C will not stop the session: {e}");
    }
}

fn finish(
    events: impl Iterator<Item = Value>,
    stream: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    for event in events {
        match outcome(&event, stream) {
            None => {}
            Some(Outcome::Segment(text)) => print(out, &text)?,
            Some(Outcome::Done(text)) => return text.map_or(Ok(()), |text| print(out, &text)),
            Some(Outcome::Fail(line)) => return Err(line),
        }
    }
    Err(CLOSED.to_owned())
}

/// Waits for the `ack` (or `error`) answering a `stop`/`cancel`.
fn acked(mut events: impl Iterator<Item = Value>) -> Result<(), String> {
    let reply = events.find_map(|event| match event["event"].as_str()? {
        "ack" => Some(Ok(())),
        "error" => Some(Err(error_line(&event))),
        _ => None,
    });
    reply.unwrap_or_else(|| Err(CLOSED.to_owned()))
}

/// `ursula record`: sends `start` (a `Request::Start`) and prints the result.
pub fn record(socket: &Path, start: Request, out: &mut dyn Write) -> Result<(), String> {
    let stream_mode = matches!(start, Request::Start { stream: true, .. });
    let stream = connect(socket)?;
    send(&stream, start)?;
    stop_on_ctrlc(&stream);
    finish(events(&stream), stream_mode, out)
}

/// `ursula toggle`: like `record`, but when another session is active, stops
/// that session by id instead and prints nothing.
pub fn toggle(socket: &Path, start: Request, out: &mut dyn Write) -> Result<(), String> {
    let stream_mode = matches!(start, Request::Start { stream: true, .. });
    let stream = connect(socket)?;
    send(&stream, start)?;
    let mut events = events(&stream).peekable();
    let busy = events
        .next_if(|e| e["event"] == "error" && e["code"] == "busy" && e["session"].is_string());
    if let Some(busy) = busy {
        let session = busy["session"].as_str().map(str::to_owned);
        send(&stream, Request::Stop { session })?;
        return acked(events);
    }
    stop_on_ctrlc(&stream);
    finish(events, stream_mode, out)
}

/// `ursula stop` / `ursula cancel`: sends `req` and waits for its `ack`.
pub fn control(socket: &Path, req: Request) -> Result<(), String> {
    let stream = connect(socket)?;
    send(&stream, req)?;
    acked(events(&stream))
}

/// `ursula file PATH`: transcribes a WAV file and prints `done.text`.
pub fn file(socket: &Path, path: &Path, out: &mut dyn Write) -> Result<(), String> {
    let abs = std::fs::canonicalize(path)
        .map_err(|e| format!("error: cannot open {}: {e}", path.display()))?
        .into_os_string()
        .into_string()
        .map_err(|p| format!("error: path is not UTF-8: {}", Path::new(&p).display()))?;
    let start = Request::Start {
        source: Source::File(abs),
        stream: false,
        hotwords: Vec::new(),
        context: None,
        postprocess: Vec::new(),
        max_ms: None,
        silence_stop_ms: None,
    };
    record(socket, start, out)
}

/// `ursula status`: prints the daemon's status line verbatim.
pub fn status(socket: &Path, out: &mut dyn Write) -> Result<(), String> {
    let stream = connect(socket)?;
    send(&stream, Request::Status)?;
    let line = lines(&stream).next().ok_or_else(|| CLOSED.to_owned())?;
    print(out, &line)
}

/// `ursula raw`: copies `input` to the socket, then half-closes it ("no more
/// requests"; the daemon still finishes any session this connection owns),
/// while copying everything the daemon sends to `out` until EOF.
pub fn raw(
    socket: &Path,
    mut input: impl Read + Send + 'static,
    out: &mut dyn Write,
) -> Result<(), String> {
    let stream = connect(socket)?;
    let mut writer = stream
        .try_clone()
        .map_err(|e| format!("error: cannot clone socket: {e}"))?;
    // Detached: if the daemon closes first, the process must not wait on stdin.
    thread::spawn(move || {
        // On a copy error the daemon is gone and the main loop sees EOF.
        let _ = io::copy(&mut input, &mut writer);
        let _ = writer.shutdown(Shutdown::Write);
    });
    io::copy(&mut &stream, out)
        .and_then(|_| out.flush())
        .map_err(|e| format!("error: cannot copy events: {e}"))
}

#[cfg(test)]
mod tests;
