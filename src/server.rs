//! Unix-socket daemon (DESIGN.md "Architecture"): startup, accept thread, and the runtime
//! loop that owns the [`Controller`] and engine and executes its outputs.
//!
//! Responsiveness under load (documented design choices, not hard real-time guarantees):
//! - `status` is out-of-band: the connection's reader answers it from the shared snapshot,
//!   unordered relative to that connection's earlier requests, and promptly only while that
//!   reader is not itself blocked enqueueing an earlier request.
//! - A `cancel`, or a full close (POLLHUP) of the owning connection, sets the per-gen cancel
//!   hint; the controller then discards that gen's queued audio without decoding. A `cancel`
//!   behind blocked audio on the same connection is head-of-line blocked: cancel from a
//!   separate connection. Hint-to-stop latency is bounded by the engine work in progress.
//! - `stop` is ordered behind the connection's own queued audio, by design.
//! - Supervision ticks run on this loop after every input and every 100 ms idle.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions, Permissions, TryLockError};
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::{self, AudioSink};
use crate::config;
use crate::engine::vad_offline::VadOfflineEngine;
use crate::mic::{self, now_ms};
use crate::postprocess::run_chain;
use crate::protocol::to_line;
use crate::session::{Controller, Input, Output};

mod conn;
#[cfg(test)]
mod tests;

use conn::{close_graceful, close_now, deliver, lock, Delivery, Shared};

/// Ingress FIFO capacity; with 1 MiB lines this bounds queued request data to ~64 MiB.
const INGRESS: usize = 64;
const TICK: Duration = Duration::from_millis(100);

pub fn serve(cfg_path: &Path, socket: &Path) -> Result<(), String> {
    now_ms();
    // Held (never unlinked) for the process lifetime: the singleton guard.
    let _lock = take_lock(socket)?;
    remove_stale_socket(socket)?;
    let cfg = config::load(cfg_path).map_err(|e| format!("error: {e}"))?;
    let t0 = Instant::now();
    let engine = VadOfflineEngine::new(&cfg).map_err(|e| format!("error: {e}"))?;
    eprintln!(
        "info: loaded {} in {} ms",
        cfg.model.name,
        t0.elapsed().as_millis()
    );
    let at = |e: std::io::Error| format!("error: cannot bind {}: {e}", socket.display());
    let listener = UnixListener::bind(socket).map_err(at)?;
    fs::set_permissions(socket, Permissions::from_mode(0o600)).map_err(at)?;
    eprintln!("info: listening on {}", socket.display());

    let ctl = Controller::new(Box::new(engine), cfg);
    let (ingress, ingress_rx) = sync_channel::<Input>(INGRESS);
    let shared = Shared {
        conns: Arc::default(),
        snapshot: Arc::new(Mutex::new(ctl.snapshot())),
        hint: ctl.cancel_hint(),
        ingress,
    };
    let accept_shared = shared.clone();
    thread::spawn(move || accept_loop(&listener, &accept_shared));
    Runtime {
        ctl,
        shared,
        mic_last_data: Arc::default(),
        stops: HashMap::new(),
        posts: HashMap::new(),
    }
    .run_forever(&ingress_rx)
}

fn take_lock(socket: &Path) -> Result<fs::File, String> {
    let mut path = socket.as_os_str().to_owned();
    path.push(".lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("error: cannot open {}: {e}", Path::new(&path).display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(format!(
            "error: another ursula serve owns {}",
            socket.display()
        )),
        Err(TryLockError::Error(e)) => Err(format!("error: cannot lock {path:?}: {e}")),
    }
}

/// Safe only under the lock: a leftover socket is from a dead daemon.
fn remove_stale_socket(socket: &Path) -> Result<(), String> {
    let shown = socket.display();
    match fs::symlink_metadata(socket) {
        Ok(m) if m.file_type().is_socket() => {
            fs::remove_file(socket).map_err(|e| format!("error: cannot remove {shown}: {e}"))
        }
        Ok(_) => Err(format!("error: {shown} exists and is not a socket")),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("error: {shown}: {e}")),
    }
}

fn accept_loop(listener: &UnixListener, shared: &Shared) {
    let mut next_id = 0;
    for stream in listener.incoming() {
        match stream {
            Ok(sock) => {
                next_id += 1;
                if let Err(e) = conn::spawn(next_id, sock, shared) {
                    eprintln!("warning: dropping conn {next_id}: {e}");
                }
            }
            Err(e) => {
                eprintln!("warning: accept failed: {e}");
                thread::sleep(TICK);
            }
        }
    }
}

/// Owns the controller (and through it the engine). Never blocks on a client and never
/// sends on ingress itself; it only hands ingress clones to the threads it spawns.
struct Runtime {
    ctl: Controller,
    shared: Shared,
    mic_last_data: Arc<AtomicU64>,
    stops: HashMap<u64, Arc<AtomicBool>>,
    posts: HashMap<u64, Arc<AtomicBool>>,
}

impl Runtime {
    fn run_forever(mut self, rx: &Receiver<Input>) -> ! {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(input) => self.run(input),
                // Disconnected is impossible: the accept thread holds a sender.
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
            let last_data_ms = self.mic_last_data.load(Ordering::Relaxed);
            self.run(Input::Tick {
                now_ms: now_ms(),
                last_data_ms,
            });
        }
    }

    /// Work list without re-entry: each batch is fully executed before the next `handle`.
    fn run(&mut self, input: Input) {
        let mut pending = VecDeque::from([input]);
        while let Some(input) = pending.pop_front() {
            if let Input::PostDone { gen, .. } = &input {
                self.posts.remove(gen);
            }
            // Peer gone: nothing more can be delivered. The controller only emits CloseConn
            // for half-closed owners, so drop the entry (and end its writer) here; idempotent.
            let gone = match &input {
                Input::ConnClosed(conn) => Some(*conn),
                _ => None,
            };
            for out in self.ctl.handle(input, now_ms()) {
                self.exec(out, &mut pending);
            }
            if let Some(conn) = gone {
                close_now(&self.shared.conns, conn);
            }
        }
    }

    fn exec(&mut self, out: Output, pending: &mut VecDeque<Input>) {
        match out {
            Output::Send { conn, out } => {
                let line = to_line(&out);
                let delivery = lock(&self.shared.conns)
                    .get(&conn)
                    .map(|e| deliver(e, line));
                if let Some(Delivery::SlowReader) = delivery {
                    eprintln!("info: closing slow reader conn {conn}");
                    close_now(&self.shared.conns, conn);
                    pending.push_back(Input::ConnClosed(conn));
                }
            }
            Output::CloseConn { conn } => close_graceful(&self.shared.conns, conn),
            Output::OpenMic { gen } => {
                let stop = self.new_flag(gen);
                let last = Arc::clone(&self.mic_last_data);
                mic::spawn_mic(None, gen, self.sink(), stop, last);
            }
            Output::StartFile { gen, reader } => {
                let stop = self.new_flag(gen);
                audio::spawn_file_feeder(reader, gen, self.sink(), stop);
            }
            Output::StopCapture { gen } => {
                if let Some(stop) = self.stops.remove(&gen) {
                    stop.store(true, Ordering::Relaxed);
                }
            }
            Output::RunPost { gen, job } => {
                let cancel = Arc::new(AtomicBool::new(false));
                self.posts.insert(gen, Arc::clone(&cancel));
                let ingress = self.shared.ingress.clone();
                thread::spawn(move || {
                    let (text, warnings) = run_chain(job, &cancel);
                    let _ = ingress.send(Input::PostDone {
                        gen,
                        text,
                        warnings,
                    });
                });
            }
            Output::CancelPost { gen } => {
                if let Some(cancel) = self.posts.remove(&gen) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            Output::Publish(s) => *lock(&self.shared.snapshot) = s,
        }
    }

    fn new_flag(&mut self, gen: u64) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        self.stops.insert(gen, Arc::clone(&stop));
        stop
    }

    fn sink(&self) -> AudioSink {
        let ingress: SyncSender<Input> = self.shared.ingress.clone();
        Box::new(move |msg| ingress.send(Input::Audio(msg)).is_ok())
    }
}
