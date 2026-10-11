//! `run_cli` (deadline-bounded child processes) and the tests of the harness itself.

use std::fs;
use std::io::{BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::harness::{drain, expect_event, lock, poll_until, scratch_dir};

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// SIGKILL every process in group `pgid` (never 0 or 1: our own group / everything).
fn kill_group(pgid: u32) {
    if pgid > 1 {
        // SAFETY: kill(2) takes only integers and touches no memory of ours.
        unsafe { kill(-(pgid as i32), 9) };
    }
}

/// Runs `cmd` in its own process group with stdout/stderr drained concurrently. On
/// timeout the whole group is killed and reaped, then this panics with `(pgid N)`.
pub fn run_cli(mut cmd: Command, stdin: Option<&[u8]>, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    let input = if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    cmd.process_group(0)
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn child");
    if let (Some(data), Some(mut w)) = (stdin, child.stdin.take()) {
        let data = data.to_vec();
        // Detached: a child that never reads stdin must not block us.
        thread::spawn(move || w.write_all(&data));
    }
    let (out, out_t) = drain(child.stdout.take().expect("stdout"));
    let (err, err_t) = drain(child.stderr.take().expect("stderr"));
    let mut status = None;
    while Instant::now() < deadline {
        status = status.or_else(|| child.try_wait().expect("try_wait"));
        if status.is_some() && out_t.is_finished() && err_t.is_finished() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let pgid = child.id();
    let Some(status) = status.filter(|_| out_t.is_finished() && err_t.is_finished()) else {
        kill_group(pgid);
        let _ = child.wait();
        panic!("run_cli: {cmd:?} timed out after {timeout:?} (pgid {pgid})");
    };
    let (stdout, stderr) = (lock(&out).clone(), lock(&err).clone());
    Output {
        status,
        stdout,
        stderr,
    }
}

/// Live (non-zombie) processes whose process group is `pgid`, from `/proc/*/stat`.
fn group_members(pgid: u32) -> usize {
    let Ok(dir) = fs::read_dir("/proc") else {
        return 0;
    };
    dir.filter_map(|e| fs::read_to_string(e.ok()?.path().join("stat")).ok())
        .filter(|stat| {
            // "pid (comm) S ppid pgrp ..."; comm may contain spaces, so split after ')'.
            let fields: Vec<_> = stat
                .rsplit_once(')')
                .map_or("", |r| r.1)
                .split_whitespace()
                .collect();
            matches!(fields.as_slice(), [state, _, pgrp, ..]
                if !matches!(*state, "Z" | "X") && pgrp.parse() == Ok(pgid))
        })
        .count()
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default()
}

type Peer = fn(UnixStream);

/// A fake daemon that accepts one connection and runs `peer` on it.
fn listener_peer(name: &str, peer: Peer) -> UnixStream {
    let path = scratch_dir().join(format!("{name}.sock"));
    let listener = UnixListener::bind(&path).expect("bind");
    thread::spawn(move || {
        if let Ok((sock, _)) = listener.accept() {
            peer(sock);
        }
    });
    UnixStream::connect(&path).expect("connect")
}

fn trickle(mut sock: UnixStream, bytes: &[u8]) {
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end && sock.write_all(bytes).is_ok() {
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn harness_timeout() {
    let peers: [(&str, Peer); 3] = [
        ("silent", |sock| {
            thread::sleep(Duration::from_secs(3));
            drop(sock);
        }),
        ("lines", |sock| trickle(sock, b"{\"event\":\"x\"}\n")),
        ("bytes", |sock| trickle(sock, b"x")),
    ];
    for (name, peer) in peers {
        let mut r = BufReader::new(listener_peer(name, peer));
        let t0 = Instant::now();
        let res = catch_unwind(AssertUnwindSafe(|| {
            expect_event(&mut r, "done", Duration::from_secs(1))
        }));
        let took = t0.elapsed();
        assert!(res.is_err(), "{name}: expect_event returned");
        assert!(took < Duration::from_millis(1500), "{name}: took {took:?}");
    }

    let mut cmd = Command::new("sh");
    cmd.args(["-c", "yes | head -c 10000000; sleep 30"]);
    let t0 = Instant::now();
    let res = catch_unwind(AssertUnwindSafe(|| {
        run_cli(cmd, None, Duration::from_secs(1))
    }));
    let took = t0.elapsed();
    let msg = panic_text(&*res.expect_err("run_cli must time out"));
    assert!(took < Duration::from_secs(3), "run_cli took {took:?}");
    let pgid: u32 = msg
        .rsplit_once("(pgid ")
        .and_then(|(_, r)| r.strip_suffix(')'))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no pgid in {msg:?}"));
    let empty = poll_until(Duration::from_secs(2), || group_members(pgid) == 0);
    assert!(
        empty,
        "group {pgid} still has {} members",
        group_members(pgid)
    );
}

#[test]
fn cli_smoke() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ursula"));
    cmd.args(["--socket", "/nonexistent.sock", "status"]);
    let out = run_cli(cmd, None, Duration::from_secs(10));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("cannot connect to /nonexistent.sock"),
        "{stderr}"
    );
}
