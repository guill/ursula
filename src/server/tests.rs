use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::conn::{close_now, deliver, send_hup_aware, write_with_deadline, ConnEntry, Delivery};
use crate::mic::now_ms;

const TEN_MB: usize = 10 << 20;

/// Writes 10 MB on a thread; the result arrives on the returned channel.
fn spawn_write(sock: UnixStream, closing: &Arc<AtomicU64>) -> mpsc::Receiver<Result<(), ()>> {
    sock.set_write_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let closing = Arc::clone(closing);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(write_with_deadline(&sock, &vec![b'x'; TEN_MB], &closing));
    });
    rx
}

#[test]
fn write_deadline_set_while_blocked() {
    // Given: a peer that never reads and a writer blocked on it.
    let (a, _peer) = UnixStream::pair().unwrap();
    let closing = Arc::new(AtomicU64::new(0));
    let rx = spawn_write(a, &closing);
    thread::sleep(Duration::from_millis(300));
    // When: graceful close begins.
    closing.store(now_ms().max(1), Ordering::Relaxed);
    // Then: the write gives up within the deadline.
    let got = rx.recv_timeout(Duration::from_millis(5500));
    assert_eq!(got, Ok(Err(())));
}

#[test]
fn write_deadline_trickle_reader() {
    // Given: a peer reading 64 bytes every 50 ms and a write already closing.
    let (a, mut peer) = UnixStream::pair().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = Arc::clone(&done);
    thread::spawn(move || {
        let mut chunk = [0; 64];
        while !peer_done.load(Ordering::Relaxed) && peer.read(&mut chunk).is_ok_and(|n| n > 0) {
            thread::sleep(Duration::from_millis(50));
        }
    });
    let closing = Arc::new(AtomicU64::new(now_ms().max(1)));
    // When: writing 10 MB.
    let rx = spawn_write(a, &closing);
    // Then: partial progress does not extend the deadline.
    let got = rx.recv_timeout(Duration::from_millis(5500));
    done.store(true, Ordering::Relaxed);
    assert_eq!(got, Ok(Err(())));
}

#[test]
fn write_peer_shutdown() {
    // Given: a non-closing write blocked on a peer that never reads.
    let (a, peer) = UnixStream::pair().unwrap();
    let rx = spawn_write(a, &Arc::new(AtomicU64::new(0)));
    thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    // When: the peer shuts down.
    peer.shutdown(Shutdown::Both).unwrap();
    // Then: the write fails promptly.
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(Err(())));
    assert!(t0.elapsed() < Duration::from_secs(1));
}

#[test]
fn deliver_full_closes() {
    // Given: a conn whose writer queue is full and some bytes already buffered for the peer.
    let (mut sock, mut peer) = UnixStream::pair().unwrap();
    sock.write_all(b"buffered\n").unwrap();
    let (tx, _rx) = sync_channel(1);
    tx.try_send("queued".to_owned()).unwrap();
    let entry = ConnEntry {
        tx,
        sock,
        closing_since_ms: Arc::default(),
    };
    // When: one more line is delivered.
    let got = deliver(&entry, "line\n".to_owned());
    // Then: it is a slow reader; closing it removes the entry and EOFs the peer.
    assert_eq!(got, Delivery::SlowReader);
    let conns = Arc::new(Mutex::new(HashMap::from([(7, entry)])));
    close_now(&conns, 7);
    assert!(conns.lock().unwrap().is_empty());
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut out = String::new();
    peer.read_to_string(&mut out).unwrap();
    assert_eq!(out, "buffered\n");
}

#[test]
fn send_hup_aware_detects_close() {
    // Given: a full channel and a sender blocked in send_hup_aware.
    let (tx, rx) = sync_channel(1);
    tx.send(1).unwrap();
    let (sock, peer) = UnixStream::pair().unwrap();
    let (hup_tx, hup_rx) = mpsc::channel();
    let sender = thread::spawn(move || {
        let fd = sock.as_raw_fd();
        let on_hup = || hup_tx.send(()).unwrap();
        let sent = send_hup_aware(&tx, 2, fd, on_hup);
        drop(sock);
        sent
    });
    // When: the peer goes away.
    drop(peer);
    // Then: on_hup fires while the send is still pending, and the send completes once
    // there is room.
    assert_eq!(hup_rx.recv_timeout(Duration::from_millis(300)), Ok(()));
    assert!(!sender.is_finished());
    assert_eq!(rx.recv(), Ok(1));
    assert_eq!(sender.join().unwrap(), Ok(()));
    assert_eq!(rx.recv(), Ok(2));
}
