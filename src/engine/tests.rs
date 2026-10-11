use super::fake::{FakeEngine, FakeMode};
use super::vad_offline::VadOfflineEngine;
use super::*;
use crate::config::{self, Config};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const REFERENCE: &str = "well i don't wish to see it any more observed phebe turning away \
    her eyes it is certainly very like the old portrait";

/// Unique temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ursula-engine-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn load(&self, model_dir: &Path, vad: &Path, extra: &str) -> Result<Config, String> {
        let path = self.0.join("config.toml");
        let text = format!("model.dir = {model_dir:?}\nvad.model = {vad:?}\n{extra}\n");
        fs::write(&path, text).unwrap();
        config::load(&path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `(model_dir, vad)` from the devShell env, or `None` after printing why.
fn e2e_env(test: &str) -> Option<(PathBuf, PathBuf)> {
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let found = var("URSULA_E2E_MODEL_DIR").zip(var("URSULA_E2E_VAD"));
    if found.is_none() {
        println!("{test}: skipped: URSULA_E2E_MODEL_DIR or URSULA_E2E_VAD unset");
    }
    found
}

fn read_wav(path: &Path) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).unwrap();
    let s = r.spec();
    assert_eq!(
        (s.sample_rate, s.channels, s.bits_per_sample),
        (16000, 1, 16)
    );
    r.samples::<i16>()
        .map(|x| f32::from(x.unwrap()) / 32768.0)
        .collect()
}

fn normalize(s: &str) -> String {
    let kept: String = s
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '\'' | ' '))
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn word_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<_>, Vec<_>) = (a.split(' ').collect(), b.split(' ').collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, wa) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, wb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(wa != wb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Streams `pcm` in `chunk`-sized pieces exactly like the controller will.
fn run(engine: &mut dyn Engine, pcm: &[f32], chunk: usize) -> Vec<(String, f32, f32)> {
    engine.begin(&[]);
    let mut residual = Vec::new();
    let mut events = Vec::new();
    for c in pcm.chunks(chunk) {
        for w in steps512(&mut residual, c) {
            events.extend(engine.accept(&w));
        }
    }
    if !residual.is_empty() {
        events.extend(engine.accept(&residual));
    }
    events.extend(engine.finish());
    events
        .into_iter()
        .map(|e| match e {
            EngineEvent::Segment { text, start, end } => (text, start, end),
            EngineEvent::Partial { text } => panic!("unexpected partial {text}"),
        })
        .collect()
}

fn assert_monotonic(segs: &[(String, f32, f32)], total: f32) {
    let mut last = 0.0;
    for (text, start, end) in segs {
        assert!(
            *start >= last && end >= start,
            "{start}..{end} after {last}: {text}"
        );
        last = *end;
    }
    assert!(last <= total, "last end {last} > duration {total}");
}

#[test]
fn real_model_wav() {
    let Some((model, vad)) = e2e_env("real_model_wav") else {
        return;
    };
    let tmp = TempDir::new();
    let mut engine = VadOfflineEngine::new(&tmp.load(&model, &vad, "").unwrap()).unwrap();
    let pcm = read_wav(&model.join("test_wavs/0.wav"));
    let segs = run(&mut engine, &pcm, 1600);
    println!("real_model_wav segments: {segs:?}");
    let text = segs
        .iter()
        .map(|s| s.0.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let dist = word_distance(&normalize(&text), REFERENCE);
    assert!(dist <= 2, "distance {dist}: {text}");
    assert_monotonic(&segs, pcm.len() as f32 / 16000.0);
}

#[test]
fn real_model_hard_split() {
    let Some((model, vad)) = e2e_env("real_model_hard_split") else {
        return;
    };
    let tmp = TempDir::new();
    let extra = "vad.max_speech_ms = 5000\nvad.min_silence_ms = 2000";
    let mut engine = VadOfflineEngine::new(&tmp.load(&model, &vad, extra).unwrap()).unwrap();
    let pcm = read_wav(&model.join("test_wavs/0.wav")).repeat(4);
    let segs = run(&mut engine, &pcm, WINDOW);
    for (text, start, end) in &segs {
        println!(
            "real_model_hard_split: {start:.3}..{end:.3} ({:.3} s) {text}",
            end - start
        );
        // At sample resolution, so f32 rounding of the seconds can't flake it.
        let samples = ((end - start) * 16000.0).round();
        assert!(samples <= 80_000.0, "{start}..{end} exceeds 5 s");
    }
    assert_monotonic(&segs, pcm.len() as f32 / 16000.0);
    let text = normalize(
        &segs
            .iter()
            .map(|s| s.0.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    // The model spells the name "Phoebe"; the LibriSpeech reference has "phebe".
    let names = text
        .split(' ')
        .filter(|w| matches!(*w, "phebe" | "phoebe"))
        .count();
    assert!(names >= 3, "{text}");
}

#[test]
fn new_fails_without_tokens() {
    let tmp = TempDir::new();
    let model = tmp.0.join("model");
    fs::create_dir_all(&model).unwrap();
    for f in [
        "encoder.onnx",
        "decoder.onnx",
        "joiner.onnx",
        "tokens.txt",
        "vad.onnx",
    ] {
        fs::write(model.join(f), "").unwrap();
    }
    let vad = model.join("vad.onnx");
    let cfg = tmp.load(&model, &vad, "").unwrap();
    fs::remove_file(model.join("tokens.txt")).unwrap();

    let loaded = tmp
        .load(&model, &vad, "")
        .and_then(|c| VadOfflineEngine::new(&c).map(drop));
    assert!(loaded.unwrap_err().contains("tokens.txt"));
    let direct = VadOfflineEngine::new(&cfg).map(drop);
    assert!(direct.unwrap_err().contains("tokens.txt"));
}

#[test]
fn chunking_invariance() {
    let pcm: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.01).sin()).collect();
    let split = |sizes: &[usize]| {
        let (mut residual, mut windows, mut at) = (Vec::new(), Vec::new(), 0);
        for &n in sizes {
            windows.extend(steps512(&mut residual, &pcm[at..at + n]));
            at += n;
        }
        assert_eq!(at, pcm.len());
        (windows, residual)
    };
    let one = split(&[48_000]);
    let steps = split(&[[512].repeat(93), vec![384]].concat());
    let small = split(&[160].repeat(300));
    assert_eq!(one.0.len(), 93);
    assert_eq!(one.1, pcm[93 * 512..]);
    assert_eq!(one, steps);
    assert_eq!(one, small);
}

#[test]
fn steps512_keeps_residual() {
    let mut residual = Vec::new();
    let windows = steps512(&mut residual, &[0.5; 1000]);
    assert_eq!((windows.len(), residual.len()), (1, 488));
}

#[test]
fn slice_bound() {
    let cases = [(0, 5), (5, 5), (11, 5), (80_000, 80_000), (80_001, 80_000)];
    for (len, max) in cases {
        let pieces = slice_pcm(len, max);
        let mut at = 0;
        for &(a, b) in &pieces {
            assert!(
                a == at && b > a && b - a <= max,
                "({len},{max}): {pieces:?}"
            );
            at = b;
        }
        assert_eq!(at, len, "({len},{max}): {pieces:?}");
    }
    assert_eq!(slice_pcm(11, 5), [(0, 5), (5, 10), (10, 11)]);
}

#[test]
fn fake_engine_every_second() {
    let (mut engine, log) = FakeEngine::new(FakeMode::EverySecond);
    let segs = run(&mut engine, &[0.1; 40_000], 1600);
    let texts: Vec<_> = segs.iter().map(|s| (s.0.as_str(), s.1, s.2)).collect();
    assert_eq!(
        texts,
        [("seg1", 0.0, 1.0), ("seg2", 1.0, 2.0), ("seg3", 2.0, 2.5)]
    );
    assert!(engine.in_speech());
    let log = log.lock().unwrap();
    assert_eq!((log.accepts, log.samples, log.finishes), (79, 40_000, 1));
}

#[test]
fn fake_engine_residual_only() {
    let (mut engine, log) = FakeEngine::new(FakeMode::ResidualOnly);
    let mut pcm = vec![0.0; 1100];
    pcm[1099] = 0.1;
    let segs = run(&mut engine, &pcm, 1600);
    assert_eq!(
        segs,
        [("seg1".to_owned(), 1024.0 / 16000.0, 1100.0 / 16000.0)]
    );
    assert!(engine.in_speech());
    assert_eq!(log.lock().unwrap().samples, 1100);
}
