//! Fake-mic sessions (plan todo 13): record/stop/toggle/cancel, data stall and the mic
//! worker bound, server death, no server, killed client and silence stop.
//! Needs `URSULA_E2E_PULSE=1` plus `pulseaudio`, `pactl` and `ffmpeg` on PATH.

use std::process::Output;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use toml::Table;

use crate::cli::run_cli;
use crate::fakemic::{Bg, FakeMic};
use crate::harness::{poll_until, Daemon, DECODE, IO_TIMEOUT};
use crate::text::assert_transcript;

const BARRIER: Duration = Duration::from_secs(5);
const WORKER_BUSY: &str = "has not shut down";

fn setup() -> Option<(Daemon, FakeMic)> {
    let mic = FakeMic::start()?;
    let env = mic.env();
    let envs: Vec<_> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let d = Daemon::start_with_env(Table::new(), &envs)?;
    Some((d, mic))
}

/// Spawns `ursula <args>` and waits for the readiness barrier: its capture stream is open.
fn record(d: &Daemon, mic: &FakeMic, args: &[&str]) -> Bg {
    let rec = Bg::spawn(d.cli(args));
    assert!(
        mic.wait_stream(true, BARRIER),
        "no capture stream in {BARRIER:?}"
    );
    rec
}

/// `record`, retried every 500 ms for up to 5 s while the old mic worker is still alive.
fn record_retry(d: &Daemon, mic: &FakeMic) -> Bg {
    let deadline = Instant::now() + BARRIER;
    loop {
        let mut rec = Bg::spawn(d.cli(&["record"]));
        while !rec.exited() {
            if mic.wait_stream(true, Duration::ZERO) {
                return rec;
            }
            assert!(
                Instant::now() < deadline,
                "record never reached the barrier"
            );
            thread::sleep(Duration::from_millis(50));
        }
        let out = rec.wait(IO_TIMEOUT);
        assert!(
            Instant::now() < deadline,
            "record kept failing: {}",
            text(&out.stderr)
        );
        thread::sleep(Duration::from_millis(500));
    }
}

fn cli(d: &Daemon, args: &[&str]) -> Output {
    run_cli(d.cli(args), None, IO_TIMEOUT)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Asserts the exit code; returns (stdout, stderr).
fn exits(out: &Output, code: i32) -> (String, String) {
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout {stdout:?} stderr {stderr:?}"
    );
    (stdout, stderr)
}

fn busy(d: &Daemon) -> Value {
    d.status()["busy"].clone()
}

/// Feeds 0.wav at real time, waits for it, then 1 s later `stop`s; returns the record output.
fn feed_wav_and_stop(d: &Daemon, mic: &mut FakeMic, rec: Bg) -> Output {
    mic.feed(Some((&d.wav, None)), 0);
    mic.wait_fed(Duration::from_secs(30));
    thread::sleep(Duration::from_secs(1));
    exits(&cli(d, &["stop"]), 0);
    rec.wait(DECODE)
}

/// Record, feed 1 s, then SIGSTOP pulseaudio; returns the record process.
fn stalled(d: &Daemon, mic: &mut FakeMic) -> Bg {
    let rec = record(d, mic, &["record"]);
    mic.feed(Some((&d.wav, Some(1))), 0);
    mic.wait_fed(Duration::from_secs(10));
    mic.signal("STOP");
    rec
}

/// SIGCONT, 1 s of zero PCM unblocks the old reader, then its stream must close (≤ 5 s).
fn unstall(mic: &mut FakeMic) {
    mic.signal("CONT");
    mic.feed(None, 1000);
    mic.wait_fed(Duration::from_secs(5));
    assert!(
        mic.wait_stream(false, BARRIER),
        "old capture stream still open"
    );
}

#[test]
fn mic_record_stop() {
    let Some((d, mut mic)) = setup() else { return };
    let rec = record(&d, &mic, &["record"]);
    let (stdout, _) = exits(&feed_wav_and_stop(&d, &mut mic, rec), 0);
    assert_transcript(&stdout);
}

#[test]
fn mic_toggle() {
    let Some((d, mut mic)) = setup() else { return };
    let rec = record(&d, &mic, &["toggle"]);
    mic.feed(Some((&d.wav, None)), 0);
    mic.wait_fed(Duration::from_secs(30));
    let (stdout, _) = exits(&cli(&d, &["toggle"]), 0);
    assert_eq!(stdout, "", "second toggle prints nothing");
    let (stdout, _) = exits(&rec.wait(DECODE), 0);
    assert_transcript(&stdout);
}

#[test]
fn mic_cancel() {
    let Some((d, mic)) = setup() else { return };
    let rec = record(&d, &mic, &["record"]);
    exits(&cli(&d, &["cancel"]), 0);
    let (_, stderr) = exits(&rec.wait(IO_TIMEOUT), 1);
    assert!(stderr.contains("cancelled"), "{stderr}");
}

#[test]
fn mic_stall() {
    let Some((d, mut mic)) = setup() else { return };
    let rec = stalled(&d, &mut mic);
    let t0 = Instant::now();
    let (_, stderr) = exits(&rec.wait(Duration::from_secs(4)), 1);
    println!(
        "mic_stall: record exited {:?} after SIGSTOP: {}",
        t0.elapsed(),
        stderr.trim()
    );
    assert!(stderr.contains("audio_unavailable"), "{stderr}");
    assert_eq!(busy(&d), false);
    let (_, stderr) = exits(&cli(&d, &["record"]), 1);
    println!("mic_stall: worker bound: {}", stderr.trim());
    assert!(
        stderr.contains("audio_unavailable") && stderr.contains(WORKER_BUSY),
        "{stderr}"
    );
    unstall(&mut mic);
    let rec = record_retry(&d, &mic);
    let (stdout, _) = exits(&feed_wav_and_stop(&d, &mut mic, rec), 0);
    assert_transcript(&stdout);
}

#[test]
fn mic_cancel_while_stalled() {
    let Some((d, mut mic)) = setup() else { return };
    let rec = stalled(&d, &mut mic);
    let t0 = Instant::now();
    exits(&cli(&d, &["cancel"]), 0);
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "cancel took {:?}",
        t0.elapsed()
    );
    let (_, stderr) = exits(&rec.wait(IO_TIMEOUT), 1);
    assert!(stderr.contains("cancelled"), "{stderr}");
    let base = d.threads();
    for attempt in 1..=3 {
        let (_, stderr) = exits(&cli(&d, &["record"]), 1);
        assert!(stderr.contains(WORKER_BUSY), "attempt {attempt}: {stderr}");
        d.threads_settle(base + 1, Duration::from_secs(1));
        println!(
            "mic_cancel_while_stalled: attempt {attempt}: threads {} (base {base})",
            d.threads()
        );
    }
    unstall(&mut mic);
    let rec = record_retry(&d, &mic);
    exits(&cli(&d, &["stop"]), 0);
    exits(&rec.wait(DECODE), 0);
}

#[test]
fn mic_server_dies() {
    let Some((d, mic)) = setup() else { return };
    let rec = record(&d, &mic, &["record"]);
    mic.signal("KILL");
    let (_, stderr) = exits(&rec.wait(IO_TIMEOUT), 1);
    println!("mic_server_dies: {}", stderr.trim());
    assert!(stderr.contains("audio_unavailable"), "{stderr}");
    assert_eq!(busy(&d), false, "daemon alive and idle");
}

#[test]
fn mic_no_server() {
    if std::env::var("URSULA_E2E_PULSE").as_deref() != Ok("1") {
        println!("skipped: URSULA_E2E_PULSE=1 is not set");
        return;
    }
    let env = [("PULSE_SERVER", "unix:/nonexistent")];
    let Some(d) = Daemon::start_with_env(Table::new(), &env) else {
        return;
    };
    let start = b"{\"op\":\"start\",\"source\":\"mic\"}\n";
    let out = run_cli(d.cli(&["raw"]), Some(start), IO_TIMEOUT);
    let (stdout, _) = exits(&out, 0);
    println!("mic_no_server: {}", stdout.trim());
    let events: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect();
    assert!(
        matches!(events.as_slice(), [e] if e["event"] == "error"
            && e["code"] == "audio_unavailable" && e["session"].is_string()),
        "{events:?}"
    );
}

#[test]
fn mic_killed_client() {
    let Some((d, mut mic)) = setup() else { return };
    let mut rec = record(&d, &mic, &["record"]);
    mic.feed(Some((&d.wav, None)), 0);
    thread::sleep(Duration::from_secs(1));
    rec.kill9();
    let t0 = Instant::now();
    let idle = poll_until(Duration::from_secs(1), || busy(&d) == false);
    println!("mic_killed_client: idle after {:?}", t0.elapsed());
    assert!(idle, "still busy 1 s after kill -9");
    let closed = mic.wait_stream(false, Duration::from_secs(1).saturating_sub(t0.elapsed()));
    println!("mic_killed_client: stream closed after {:?}", t0.elapsed());
    assert!(closed, "capture stream still listed 1 s after kill -9");
}

#[test]
fn mic_silence_stop() {
    let Some((d, mut mic)) = setup() else { return };
    let rec = record(&d, &mic, &["record", "--silence-stop", "1500"]);
    mic.feed(Some((&d.wav, None)), 4000);
    let (stdout, _) = exits(&rec.wait(DECODE), 0);
    assert_transcript(&stdout);
}
