//! Session ownership and connection lifecycle: busy, cancel, drops, half-close, framing.

use std::io::Write;
use std::net::Shutdown;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use toml::Table;

use crate::harness::{audio_lines, poll_until, push_start, Conn, Daemon, DECODE, IO_TIMEOUT};

/// Retries `start push` on `c` (busy → retry) until `started` or `timeout`.
fn start_within(c: &mut Conn, timeout: Duration) -> Duration {
    let t0 = Instant::now();
    loop {
        c.send(&push_start());
        let ev = crate::harness::expect_event_any(&mut c.r, &["started", "error"], IO_TIMEOUT);
        if ev["event"] == "started" {
            return t0.elapsed();
        }
        assert_eq!(ev["code"], "busy", "{ev}");
        assert!(t0.elapsed() < timeout, "still busy after {timeout:?}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn busy_and_cancel() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let (mut a, mut b) = (d.connect(), d.connect());
    let sid = a.start(&push_start());
    b.send(&push_start());
    let busy = b.expect("error", IO_TIMEOUT);
    assert_eq!(
        (&busy["code"], &busy["session"]),
        (&json!("busy"), &json!(sid))
    );
    b.send(&json!({"op": "cancel"}));
    let ack = b.expect("ack", IO_TIMEOUT);
    assert_eq!(
        (&ack["op"], &ack["session"]),
        (&json!("cancel"), &json!(sid))
    );
    assert_eq!(a.expect("cancelled", IO_TIMEOUT)["session"], sid);
}

#[test]
fn drop_owner() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let mut a = d.connect();
    a.start(&push_start());
    drop(a);
    let took = start_within(&mut d.connect(), Duration::from_secs(1));
    println!("drop_owner: next start after {took:?}");
}

#[test]
fn half_close_vs_kill() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let mut a = d.connect();
    let sid = a.start(&push_start());
    a.send_raw(&audio_lines(&d.pcm()[..16_000]));
    a.w.shutdown(Shutdown::Write).expect("half-close");
    let mut b = d.connect();
    b.send(&json!({"op": "stop", "session": sid}));
    b.expect("ack", IO_TIMEOUT);
    assert_eq!(a.expect("done", DECODE)["session"], sid);

    let mut c = d.connect();
    c.start(&push_start());
    drop(c);
    let idle = poll_until(Duration::from_secs(1), || d.status()["busy"] == false);
    assert!(
        idle,
        "still busy 1 s after the owner dropped: {}",
        d.status()
    );
}

#[test]
fn immediate_requests() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let baseline = d.threads();
    for n in 0..200 {
        let mut c = d.connect();
        let start = json!({"op": "start", "source": "push", "id": n});
        c.send_raw(&format!("{start}\n{{\"op\":\"cancel\"}}\n"));
        let t0 = Instant::now();
        assert_eq!(c.expect("started", IO_TIMEOUT)["id"], n);
        c.expect("cancelled", IO_TIMEOUT.saturating_sub(t0.elapsed()));
    }
    for _ in 0..200 {
        drop(d.connect());
    }
    assert_eq!(d.status()["event"], "status");
    d.threads_settle(baseline + 2, Duration::from_secs(5));
}

/// Sends 2 MiB of `a` without a newline; expects `line too long` then the close.
fn send_long_line(c: &mut Conn) {
    let mut w = c.w.try_clone().expect("clone");
    // The daemon stops reading after 1 MiB + 2 bytes, so the tail may fail: ignore it.
    let writer = thread::spawn(move || w.write_all(&vec![b'a'; 2 << 20]).is_ok());
    let err: Value = c.expect("error", IO_TIMEOUT);
    assert_eq!(err["code"], "bad_request", "{err}");
    assert_eq!(err["message"], "line too long", "{err}");
    c.read_to_close(IO_TIMEOUT);
    let _ = writer.join();
}

#[test]
fn long_line() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    send_long_line(&mut d.connect());

    let mut owner = d.connect();
    let sid = owner.start(&push_start());
    send_long_line(&mut owner);
    assert!(
        d.wait_log(&format!("session {sid} cancelled"), IO_TIMEOUT),
        "{}",
        d.log()
    );
    start_within(&mut d.connect(), IO_TIMEOUT);
}
