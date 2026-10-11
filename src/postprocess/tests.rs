use super::*;
use std::fs;
use std::path::PathBuf;

const ECHO_X: &str = r#"cat >/dev/null; echo '{"text":"X"}'"#;

fn step(command: &[&str], timeout_ms: u64) -> (String, Postprocessor) {
    let command = command.iter().map(|s| s.to_string()).collect();
    (
        "t".into(),
        Postprocessor {
            command,
            timeout_ms,
        },
    )
}

fn sh(script: &str, timeout_ms: u64) -> (String, Postprocessor) {
    step(&["sh", "-c", script], timeout_ms)
}

fn job(text: &str, chain: Vec<(String, Postprocessor)>) -> PostJob {
    PostJob {
        text: text.into(),
        context: None,
        hotwords: vec![],
        segments: vec![],
        chain,
    }
}

/// Runs the chain uncancelled; returns (text, warnings, elapsed).
fn run(job: PostJob) -> (String, Vec<String>, Duration) {
    let t0 = Instant::now();
    let (text, warnings) = run_chain(job, &AtomicBool::new(false));
    (text, warnings, t0.elapsed())
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ursula-pp-{}-{name}", std::process::id()))
}

/// `pgrep`-style /proc scan: (pid, pgrp) of every process that is not a zombie.
fn live_procs() -> Vec<(String, String)> {
    let entries = fs::read_dir("/proc").unwrap().flatten();
    let procs = entries.filter_map(|e| {
        let stat = fs::read_to_string(e.path().join("stat")).ok()?;
        let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
        match fields.as_slice() {
            [state, _ppid, pgrp, ..] if !matches!(*state, "Z" | "X") => Some((
                e.file_name().to_string_lossy().into_owned(),
                pgrp.to_string(),
            )),
            _ => None,
        }
    });
    procs.collect()
}

#[test]
fn chain_feeds_each_output_to_the_next_step() {
    let (text, warnings, _) = run(job("in", vec![sh(ECHO_X, 5000), step(&["cat"], 5000)]));
    assert_eq!((text.as_str(), warnings), ("X", vec![]));
}

#[test]
fn stdin_carries_the_protocol_json() {
    let path = tmp("input");
    let script = r#"cat >"$1"; echo '{"text":"ok"}'"#;
    let mut j = job(
        "in",
        vec![step(
            &["sh", "-c", script, "sh", path.to_str().unwrap()],
            5000,
        )],
    );
    j.context = Some("ctx".into());
    j.hotwords = vec!["w".into()];
    j.segments = vec![Segment {
        text: "a".into(),
        start: 0.0,
        end: 1.5,
    }];
    assert_eq!(run(j).0, "ok");
    let sent: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let _ = fs::remove_file(&path);
    let segments = serde_json::json!([{"text": "a", "start": 0.0, "end": 1.5}]);
    let expected = serde_json::json!({"text": "in", "context": "ctx", "hotwords": ["w"], "segments": segments});
    assert_eq!(sent, expected);
}

#[test]
fn huge_timeout_does_not_panic() {
    let (text, warnings, _) = run(job("in", vec![sh(ECHO_X, u64::MAX)]));
    assert_eq!((text.as_str(), warnings), ("X", vec![]));
}

#[test]
fn nonzero_exit_warns_and_keeps_text() {
    let (text, warnings, _) = run(job("in", vec![sh("exit 3", 5000)]));
    assert_eq!(
        (text.as_str(), warnings),
        ("in", vec!["postprocessor t failed: exit 3".into()])
    );
}

#[test]
fn slow_step_times_out_within_a_second() {
    let (text, warnings, elapsed) = run(job("in", vec![sh("sleep 5", 200)]));
    assert_eq!(
        (text.as_str(), warnings),
        ("in", vec!["postprocessor t timed out after 200 ms".into()])
    );
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

#[test]
fn descendant_holds_stdout() {
    let path = tmp("ids");
    let script = r#"sleep 30 & echo "$$ $!" >"$1"; echo '{"text":"Y"}'"#;
    let chain = vec![step(
        &["sh", "-c", script, "sh", path.to_str().unwrap()],
        500,
    )];
    let (text, warnings, elapsed) = run(job("in", chain));
    assert_eq!((text.as_str(), warnings), ("Y", vec![]));
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    let ids = fs::read_to_string(&path).unwrap();
    let _ = fs::remove_file(&path);
    // $$ is the group id (process_group(0)); $! is the background sleep, which
    // must be gone too, so a missing process group cannot pass vacuously.
    let (pgid, bg) = ids.trim().split_once(' ').unwrap();
    let deadline = Instant::now() + Duration::from_millis(500);
    while live_procs()
        .iter()
        .any(|(pid, pgrp)| pgrp == pgid || pid == bg)
    {
        assert!(
            Instant::now() < deadline,
            "process group {pgid} still alive"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn never_reads_stdin() {
    let big = "a".repeat(300_000);
    let (text, warnings, elapsed) = run(job(&big, vec![sh("sleep 30", 300)]));
    assert_eq!(
        warnings,
        vec!["postprocessor t timed out after 300 ms".to_string()]
    );
    assert!(
        text == big && elapsed < Duration::from_secs(1),
        "{elapsed:?}"
    );
}

#[test]
fn output_overflow() {
    let script = "cat >/dev/null; head -c 2000000 /dev/zero";
    let (text, warnings, _) = run(job("in", vec![sh(script, 5000)]));
    assert_eq!(
        (text.as_str(), warnings),
        ("in", vec!["postprocessor t invalid output".into()])
    );
}

#[test]
fn cancel_returns_quickly_and_next_run_succeeds() {
    let cancel = AtomicBool::new(false);
    let t0 = Instant::now();
    let (text, warnings) = thread::scope(|s| {
        s.spawn(|| {
            thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::Relaxed);
        });
        run_chain(job("in", vec![sh("sleep 30", 60_000)]), &cancel)
    });
    let elapsed = t0.elapsed();
    assert_eq!((text.as_str(), warnings), ("in", vec![]));
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    assert_eq!(run(job("in", vec![sh(ECHO_X, 5000)])).0, "X");
}

#[test]
fn invalid_json_or_missing_text_warns() {
    for script in [
        "cat >/dev/null; echo nope",
        r#"cat >/dev/null; echo '{"text":1}'"#,
    ] {
        let (text, warnings, _) = run(job("in", vec![sh(script, 5000)]));
        assert_eq!(
            (text.as_str(), warnings),
            ("in", vec!["postprocessor t invalid output".into()])
        );
    }
}

#[test]
fn nonexistent_binary_warns() {
    let (text, warnings, _) = run(job("in", vec![step(&["/nonexistent/ursula-pp"], 5000)]));
    assert_eq!(text, "in");
    assert!(
        warnings[0].starts_with("postprocessor t could not start: "),
        "{warnings:?}"
    );
}
