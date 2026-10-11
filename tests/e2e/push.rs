//! Push sessions: streaming, request ordering, durations, segment limits, silence stop.

use std::thread;

use serde_json::{json, Value};
use toml::Table;

use crate::harness::{audio_lines, push_start, tbl, Conn, Daemon, DECODE};
use crate::text::assert_transcript;

const STOP: &str = "{\"op\":\"stop\"}\n";

/// Samples → `audio_ms` as the daemon counts it.
fn ms(samples: usize) -> u64 {
    (samples * 1000 / 16_000) as u64
}

/// Collects `segment` events until `done`; returns (segments, done).
fn segments_until_done(c: &mut Conn) -> (Vec<Value>, Value) {
    let mut segments = Vec::new();
    loop {
        let ev = crate::harness::expect_event_any(&mut c.r, &["segment", "done"], DECODE);
        if ev["event"] == "done" {
            return (segments, ev);
        }
        segments.push(ev);
    }
}

fn span(seg: &Value) -> (f64, f64) {
    (
        seg["start"].as_f64().expect("start"),
        seg["end"].as_f64().expect("end"),
    )
}

#[test]
fn push_stream() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let mut c = d.connect();
    c.start(&json!({"op": "start", "source": "push", "stream": true}));
    c.send_raw(&audio_lines(&d.pcm()));
    c.send_raw(STOP);
    let (segments, done) = segments_until_done(&mut c);
    assert!(!segments.is_empty(), "no segment before done: {done}");
    assert_eq!(done["reason"], "stop");
    for pair in segments.windows(2) {
        let ((s0, e0), (s1, e1)) = (span(&pair[0]), span(&pair[1]));
        assert!(s0 <= e0 && e0 <= s1 && s1 <= e1, "not monotonic: {pair:?}");
    }
    assert_transcript(done["text"].as_str().expect("text"));
}

/// Writes `burst` from a thread in one go while the caller reads.
fn burst(c: &Conn, burst: String) -> thread::JoinHandle<()> {
    let mut w = c.w.try_clone().expect("clone");
    thread::spawn(move || std::io::Write::write_all(&mut w, burst.as_bytes()).expect("burst"))
}

#[test]
fn push_order() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let pcm = d.pcm();
    let mut c = d.connect();
    let writer = burst(&c, format!("{}\n{}{STOP}", push_start(), audio_lines(&pcm)));
    let done = c.expect("done", DECODE);
    writer.join().expect("writer");
    assert_transcript(done["text"].as_str().expect("text"));
    let audio_ms = done["audio_ms"].as_u64().expect("audio_ms");
    assert!(
        audio_ms.abs_diff(ms(pcm.len())) <= 1,
        "{audio_ms} vs {}",
        ms(pcm.len())
    );
}

#[test]
fn push_then_restart() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let pcm = d.pcm();
    let mut c = d.connect();
    let start = push_start();
    let text = format!(
        "{start}\n{}{STOP}{start}\n{}{STOP}",
        audio_lines(&pcm),
        audio_lines(&[0; 16_000])
    );
    let writer = burst(&c, text);
    let first = c.expect("done", DECODE);
    let second = c.expect("done", DECODE);
    writer.join().expect("writer");
    assert_transcript(first["text"].as_str().expect("text"));
    assert_eq!(second["text"], "", "{second}");
    assert_eq!(second["audio_ms"], 1000, "{second}");
}

#[test]
fn exact_duration() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let mut c = d.connect();
    c.start(&push_start());
    c.send_raw(&audio_lines(&d.pcm()[..16_000]));
    c.send_raw(STOP);
    let done = c.expect("done", DECODE);
    assert_eq!(done["audio_ms"], 1000, "{done}");
}

#[test]
fn long_speech_split() {
    let over = tbl("vad = {max_speech_ms = 5000, min_silence_ms = 2000}");
    let Some(d) = Daemon::start(over) else { return };
    let pcm = d.pcm();
    let mut c = d.connect();
    c.start(&json!({"op": "start", "source": "push", "stream": true}));
    for _ in 0..4 {
        c.send_raw(&audio_lines(&pcm));
    }
    c.send_raw(STOP);
    let (segments, done) = segments_until_done(&mut c);
    assert!(!segments.is_empty(), "no segments: {done}");
    for seg in &segments {
        let (start, end) = span(seg);
        assert!(end - start <= 5.0, "segment longer than 5 s: {seg}");
    }
}

#[test]
fn silence_stop() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let mut c = d.connect();
    c.start(&json!({"op": "start", "source": "push", "silence_stop_ms": 1500}));
    c.send_raw(&audio_lines(&d.pcm()));
    c.send_raw(&audio_lines(&[0; 48_000]));
    let done = c.expect("done", DECODE);
    assert_eq!(done["reason"], "silence", "{done}");
}
