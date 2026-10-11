//! Per-connection threads and teardown. Linux `poll` semantics: a full peer close sets
//! POLLHUP, `shutdown(SHUT_WR)` alone does not. Read-EOF (half-close) never cancels; a full
//! close, a write error or a slow reader cancels the owned session and closes the conn.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use crate::mic::now_ms;
use crate::protocol::MAX_LINE;
use crate::protocol::{parse_line, parse_session_id, to_line, ErrorCode, Event, Out, Request};
use crate::session::{ConnId, Input, Snapshot};
use crate::sys;

/// Writer queue capacity; a full queue means a slow reader.
const OUTBOUND: usize = 1024;
/// Hard cap on flushing a gracefully closing connection.
const CLOSE_DEADLINE_MS: u64 = 5000;

pub type Conns = Arc<Mutex<HashMap<ConnId, ConnEntry>>>;

pub struct ConnEntry {
    pub tx: SyncSender<String>,
    pub sock: UnixStream,
    /// 0 = not closing, else the `now_ms` when graceful close began.
    pub closing_since_ms: Arc<AtomicU64>,
}

/// State shared by the runtime, the accept thread and every reader.
#[derive(Clone)]
pub struct Shared {
    pub conns: Conns,
    pub snapshot: Arc<Mutex<Snapshot>>,
    pub hint: Arc<AtomicU64>,
    pub ingress: SyncSender<Input>,
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    SlowReader,
    Gone,
}

/// Non-blocking enqueue to the conn's writer; the caller closes a `SlowReader`.
pub fn deliver(entry: &ConnEntry, line: String) -> Delivery {
    match entry.tx.try_send(line) {
        Ok(()) => Delivery::Sent,
        Err(TrySendError::Full(_)) => Delivery::SlowReader,
        Err(TrySendError::Disconnected(_)) => Delivery::Gone,
    }
}

/// Immediate teardown; the shutdown also fails any write blocked in the writer.
pub fn close_now(conns: &Conns, conn: ConnId) {
    let entry = lock(conns).remove(&conn);
    if let Some(e) = entry {
        let _ = e.sock.shutdown(Shutdown::Both);
    }
}

/// Flush-then-close; the writer enforces [`CLOSE_DEADLINE_MS`] itself.
pub fn close_graceful(conns: &Conns, conn: ConnId) {
    let entry = lock(conns).remove(&conn);
    if let Some(e) = entry {
        e.closing_since_ms.store(now_ms().max(1), Ordering::Relaxed);
    }
}

/// Registers the conn, then spawns its writer and reader, so no reply or teardown is lost.
pub fn spawn(id: ConnId, sock: UnixStream, shared: &Shared) -> std::io::Result<()> {
    let (tx, rx) = sync_channel(OUTBOUND);
    let closing = Arc::new(AtomicU64::new(0));
    let entry = ConnEntry {
        tx: tx.clone(),
        sock: sock.try_clone()?,
        closing_since_ms: Arc::clone(&closing),
    };
    let wsock = sock.try_clone()?;
    lock(&shared.conns).insert(id, entry);
    thread::spawn(move || writer(&wsock, &rx, &closing));
    let reader = Reader {
        id,
        sock,
        tx,
        shared: shared.clone(),
    };
    thread::spawn(move || reader.run());
    Ok(())
}

fn expired(closing: &AtomicU64) -> bool {
    let since = closing.load(Ordering::Relaxed);
    since != 0 && now_ms().saturating_sub(since) > CLOSE_DEADLINE_MS
}

/// The only thread that writes to its socket. Never sends on ingress.
fn writer(sock: &UnixStream, rx: &Receiver<String>, closing: &AtomicU64) {
    let _ = sock.set_write_timeout(Some(Duration::from_millis(200)));
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                if write_with_deadline(sock, line.as_bytes(), closing).is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) if closing.load(Ordering::Relaxed) != 0 => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if expired(closing) {
            break;
        }
    }
    let _ = sock.shutdown(Shutdown::Both);
}

/// Writes all of `buf`; fails once a graceful close exceeds its deadline (checked before
/// every attempt), on `Ok(0)`, or on any error other than a write timeout.
pub fn write_with_deadline(
    mut sock: &UnixStream,
    buf: &[u8],
    closing: &AtomicU64,
) -> Result<(), ()> {
    let mut off = 0;
    while off < buf.len() {
        if expired(closing) {
            return Err(());
        }
        match sock.write(&buf[off..]) {
            Ok(0) => return Err(()),
            Ok(n) => off += n,
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

/// Blocking-with-retry send that keeps order. While the queue is full it watches `fd` for
/// HUP (50 ms polls) and calls `on_hup` once, then backs off 10 ms between retries.
/// `Err` only when the receiver is gone.
pub fn send_hup_aware<T>(
    tx: &SyncSender<T>,
    msg: T,
    fd: RawFd,
    on_hup: impl FnOnce(),
) -> Result<(), ()> {
    let mut msg = msg;
    let mut on_hup = Some(on_hup);
    loop {
        match tx.try_send(msg) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(_)) => return Err(()),
            Err(TrySendError::Full(m)) => msg = m,
        }
        if on_hup.is_none() {
            thread::sleep(Duration::from_millis(10));
        } else if let Some(f) = on_hup.take_if(|_| sys::wait_hup(fd, 50)) {
            f();
        }
    }
}

struct Reader {
    id: ConnId,
    sock: UnixStream,
    tx: SyncSender<String>,
    shared: Shared,
}

impl Reader {
    fn run(self) {
        let mut lines = BufReader::new(&self.sock);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let read = (&mut lines)
                .take(MAX_LINE as u64 + 2)
                .read_until(b'\n', &mut buf);
            match read {
                Err(_) => return self.hup(),
                Ok(0) => break,
                Ok(_) => {}
            }
            let terminated = buf.ends_with(b"\n");
            if buf.len() > MAX_LINE + 1 || (!terminated && buf.len() > MAX_LINE) {
                self.reply(None, bad_request("line too long".to_owned()));
                close_graceful(&self.shared.conns, self.id);
                return self.hup();
            }
            self.line(&buf);
        }
        self.enqueue(Input::ReadClosed(self.id));
        while !sys::wait_hup(self.sock.as_raw_fd(), 200) {}
        self.hup();
    }

    /// Full close (or read error): hint the owned session, then report `ConnClosed`.
    fn hup(&self) {
        self.hint_if_owner();
        self.enqueue(Input::ConnClosed(self.id));
    }

    fn line(&self, buf: &[u8]) {
        let Ok(text) = std::str::from_utf8(buf) else {
            return self.reply(None, bad_request("invalid UTF-8".to_owned()));
        };
        let env = match parse_line(text) {
            Ok(env) => env,
            Err((id, msg)) => return self.reply(id, bad_request(msg)),
        };
        match &env.req {
            Request::Status => return self.reply(env.id, status(&lock(&self.shared.snapshot))),
            Request::Cancel { session } => {
                let gen = lock(&self.shared.snapshot).active_gen;
                if session
                    .as_deref()
                    .is_none_or(|s| parse_session_id(s) == Some(gen))
                {
                    self.shared.hint.store(gen, Ordering::Relaxed);
                }
            }
            Request::Start { .. } | Request::Audio { .. } | Request::Stop { .. } => {}
        }
        self.enqueue(Input::Request { conn: self.id, env });
    }

    fn hint_if_owner(&self) {
        let snap = lock(&self.shared.snapshot);
        if snap.owner == Some(self.id) {
            self.shared.hint.store(snap.active_gen, Ordering::Relaxed);
        }
    }

    fn enqueue(&self, input: Input) {
        // Err only if the runtime is gone, which ends the process anyway.
        let fd = self.sock.as_raw_fd();
        let _ = send_hup_aware(&self.shared.ingress, input, fd, || self.hint_if_owner());
    }

    fn reply(&self, id: Option<serde_json::Value>, event: Event) {
        if let Err(TrySendError::Full(_)) = self.tx.try_send(to_line(&Out { event, id })) {
            eprintln!("info: closing slow reader conn {}", self.id);
            close_now(&self.shared.conns, self.id);
        }
    }
}

fn bad_request(message: String) -> Event {
    Event::Error {
        code: ErrorCode::BadRequest,
        message,
        session: None,
    }
}

fn status(s: &Snapshot) -> Event {
    Event::Status {
        model: s.model.clone(),
        busy: s.busy,
        version: s.version.clone(),
        capabilities: s.capabilities,
        session: s.session.clone(),
    }
}
