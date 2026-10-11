//! File sessions, post-processing and the CLI file/raw paths.

use serde_json::json;
use toml::Table;

use crate::cli::run_cli;
use crate::harness::{Daemon, DECODE, IO_TIMEOUT};
use crate::text::assert_transcript;

#[test]
fn file_cli() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let wav = d.wav.to_str().expect("utf-8 path").to_owned();
    let out = run_cli(d.cli(&["file", &wav]), None, DECODE);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "ursula file failed: {stderr}");
    assert_transcript(&String::from_utf8_lossy(&out.stdout));
}

#[test]
fn postprocess() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let mut c = d.connect();
    c.send(&json!({"op": "start", "source": {"file": d.wav}, "postprocess": ["upper", "bad"]}));
    let done = c.expect("done", DECODE);
    assert_eq!(done["text"], "POST");
    assert_transcript(done["raw_text"].as_str().expect("raw_text"));
    let warnings = done["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].as_str().unwrap_or_default().contains("bad"),
        "{warnings:?}"
    );
}

#[test]
fn bad_wav() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let path = d.dir.join("44k.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).expect("create wav");
    for _ in 0..4410 {
        w.write_sample(0i16).expect("write sample");
    }
    w.finalize().expect("finalize wav");
    let mut c = d.connect();
    c.send(&json!({"op": "start", "source": {"file": path}}));
    let err = c.expect("error", IO_TIMEOUT);
    assert_eq!(err["code"], "bad_audio", "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap_or_default()
            .contains("ffmpeg"),
        "{err}"
    );
}

#[test]
fn relative_path() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let mut c = d.connect();
    c.send(&json!({"op": "start", "source": {"file": "x.wav"}}));
    let err = c.expect("error", IO_TIMEOUT);
    assert_eq!(err["code"], "bad_request", "{err}");
}

#[test]
fn max_ms() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let mut c = d.connect();
    c.send(&json!({"op": "start", "source": {"file": d.wav}, "max_ms": 1000}));
    let done = c.expect("done", DECODE);
    assert_eq!(done["reason"], "max_duration", "{done}");
    assert_eq!(done["audio_ms"], 1000, "{done}");
}

#[test]
fn raw_finite_pipe() {
    let Some(d) = Daemon::start(Table::new()) else {
        return;
    };
    let line = format!("{}\n", json!({"op": "start", "source": {"file": d.wav}}));
    let out = run_cli(d.cli(&["raw"]), Some(line.as_bytes()), DECODE);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains(r#""event":"done""#), "{stdout}");
}
