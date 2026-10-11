//! Responsiveness and teardown under load: saturating producers, slow readers, disconnects.

use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;
use toml::Table;

use crate::harness::{audio_lines, push_start, Conn, Daemon, IO_TIMEOUT};

/// Overall deadline of every producer loop, on top of the 5 s per-write timeout.
const PRODUCER_DEADLINE: Duration = Duration::from_secs(15);

/// Writes each item of `items` (cycled while `cycle`) until `stop`, a write error or
/// [`PRODUCER_DEADLINE`]. Returns how many items were written.
fn producer(
    c: &Conn,
    items: Vec<String>,
    cycle: bool,
    stop: &Arc<AtomicBool>,
) -> JoinHandle<usize> {
    let mut w: UnixStream = c.w.try_clone().expect("clone");
    let stop = Arc::clone(stop);
    thread::spawn(move || {
        let deadline = Instant::now() + PRODUCER_DEADLINE;
        let n = if cycle { usize::MAX } else { items.len() };
        let mut sent = 0;
        for item in items.iter().cycle().take(n) {
            let live = !stop.load(Ordering::Relaxed) && Instant::now() < deadline;
            if !live || w.write_all(item.as_bytes()).is_err() {
                break;
            }
            sent += 1;
        }
        sent
    })
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(|l| format!("{l}\n")).collect()
}

/// Starts a push session on a new connection fed with 0.wav ×5 as fast as possible.
fn loaded(d: &Daemon, stop: &Arc<AtomicBool>) -> (Conn, String, JoinHandle<usize>) {
    let mut a = d.connect();
    let sid = a.start(&push_start());
    let audio = lines(&audio_lines(&d.pcm()).repeat(5));
    let p = producer(&a, audio, false, stop);
    thread::sleep(Duration::from_secs(1));
    (a, sid, p)
}

#[test]
fn status_under_load() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (_a, sid, p) = loaded(&d, &stop);
    let mut b = d.connect();
    for _ in 0..5 {
        let t0 = Instant::now();
        b.send(&json!({"op": "status"}));
        let status = b.expect("status", IO_TIMEOUT);
        let took = t0.elapsed();
        println!("status_under_load: reply in {took:?}");
        assert_eq!(
            (&status["busy"], &status["session"]),
            (&json!(true), &json!(sid))
        );
        assert!(took < Duration::from_millis(500), "status took {took:?}");
        thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, Ordering::Relaxed);
    p.join().expect("producer");
}

#[test]
fn cancel_under_load() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (mut a, sid, p) = loaded(&d, &stop);
    let mut b = d.connect();
    let t0 = Instant::now();
    b.send(&json!({"op": "cancel"}));
    assert_eq!(
        a.expect("cancelled", Duration::from_secs(5))["session"],
        sid
    );
    println!("cancel_under_load: cancelled after {:?}", t0.elapsed());
    b.expect("ack", IO_TIMEOUT);
    stop.store(true, Ordering::Relaxed);
    p.join().expect("producer");
}

#[test]
fn disconnect_under_load() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let stop = Arc::new(AtomicBool::new(false));
    let mut a = d.connect();
    let sid = a.start(&push_start());
    let p = producer(&a, lines(&audio_lines(&d.pcm())), true, &stop);
    thread::sleep(Duration::from_secs(3));
    let status = d.status();
    assert_eq!(
        (&status["busy"], &status["session"]),
        (&json!(true), &json!(sid))
    );
    stop.store(true, Ordering::Relaxed);
    println!(
        "disconnect_under_load: producer wrote {} chunks",
        p.join().expect("producer")
    );
    drop(a);
    let t0 = Instant::now();
    let idle = crate::harness::poll_until(Duration::from_secs(5), || d.status()["busy"] == false);
    assert!(idle, "still busy 5 s after disconnect");
    let line = format!("info: session {sid} cancelled (connection closed)");
    let remaining = Duration::from_secs(5).saturating_sub(t0.elapsed());
    assert!(
        d.wait_log(&line, remaining),
        "no {line:?} in log:\n{}",
        d.log()
    );
    d.connect().start(&push_start());
}

/// Floods a never-reading conn with `req` until the daemon drops it as a slow reader.
fn outbound_full(req: &str) {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let baseline = d.threads();
    let stop = Arc::new(AtomicBool::new(false));
    let mut a = d.connect();
    let mut b = d.connect();
    let p = producer(&a, vec![format!("{req}\n"); 5000], false, &stop);
    while !p.is_finished() {
        b.send(&json!({"op": "status"}));
        b.expect("status", IO_TIMEOUT);
        thread::sleep(Duration::from_millis(100));
    }
    println!(
        "outbound_full: producer wrote {} lines",
        p.join().expect("producer")
    );
    let replies = a.read_to_close(PRODUCER_DEADLINE).len();
    println!("outbound_full: {replies} buffered replies before close");
    b.send(&json!({"op": "status"}));
    b.expect("status", IO_TIMEOUT);
    d.threads_settle(baseline + 2, Duration::from_secs(5));
    assert!(
        d.log().contains("slow reader"),
        "no slow reader log:\n{}",
        d.log()
    );
}

#[test]
fn outbound_full_runtime_path() {
    outbound_full(r#"{"op":"stop","session":"s-999999"}"#);
}

#[test]
fn outbound_full_reader_path() {
    outbound_full(r#"{"op":"status"}"#);
}

#[test]
fn graceful_close_nonreader() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let baseline = d.threads();
    let mut a = d.connect();
    a.send_raw(&"{\"op\":\"status\"}\n".repeat(1000));
    a.w.shutdown(Shutdown::Write).expect("half-close");
    d.threads_settle(baseline + 2, Duration::from_secs(8));
    drop(a);
}
