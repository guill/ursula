//! Stop-to-text latency and resident memory (plan todo 14). Ignored by default; run with
//! `cargo test --test e2e bench -- --ignored --nocapture`. Prints one JSON line.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::harness::{audio_lines, push_start, tbl, Daemon, DECODE};
use crate::text::assert_transcript;
use crate::wire::CHUNK;

const RUNS: usize = 5;
/// Pinned rather than the `min(4, CPUs)` default, so the report names what was measured.
const THREADS: i32 = 4;

/// `key` (e.g. `VmRSS`) from `/proc/<pid>/status`, in MiB.
fn status_mib(pid: u32, key: &str) -> u64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("read /proc status");
    let value = status
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .unwrap_or_else(|| panic!("{key} missing from /proc/{pid}/status"));
    let kb: u64 = value
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .expect("kB value");
    kb / 1024
}

/// One push session fed at real time (one 100 ms chunk per 100 ms); wall ms from `stop`
/// to `done`.
fn stop_to_done_ms(d: &Daemon, pcm: &[i16]) -> u128 {
    let mut c = d.connect();
    c.start(&push_start());
    for chunk in pcm.chunks(CHUNK) {
        c.send_raw(&audio_lines(chunk));
        thread::sleep(Duration::from_millis(100));
    }
    let stopped = Instant::now();
    c.send_raw("{\"op\":\"stop\"}\n");
    let done = c.expect("done", DECODE);
    let ms = stopped.elapsed().as_millis();
    assert_transcript(done["text"].as_str().expect("text"));
    ms
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture"]
fn bench() {
    let Some(d) = Daemon::start(tbl(&format!("model.threads = {THREADS}"))) else {
        return;
    };
    let idle_rss = status_mib(d.pid(), "VmRSS");
    let pcm = d.pcm();
    let mut runs: Vec<u128> = (0..RUNS).map(|_| stop_to_done_ms(&d, &pcm)).collect();
    let (rss, hwm) = (status_mib(d.pid(), "VmRSS"), status_mib(d.pid(), "VmHWM"));
    let in_order = runs.clone();
    runs.sort_unstable();
    println!(
        "{}",
        json!({
            "bench": "stop_to_done_ms",
            "runs": in_order,
            "min": runs[0],
            "median": runs[RUNS / 2],
            "max": runs[RUNS - 1],
            "idle_rss_mb": idle_rss,
            "rss_mb": rss,
            "hwm_mb": hwm,
            "threads": THREADS,
        })
    );
}
