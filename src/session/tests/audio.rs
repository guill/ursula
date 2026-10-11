//! Feeding: caps, silence stop, streaming, cancel hint and sample accounting.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use super::*;
use crate::engine::EngineEvent;
use crate::protocol::Capabilities;

#[test]
fn t07_max_ms_over_small_pushes() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, r#"{"op":"start","source":"push","max_ms":100}"#);
    let mut done = None;
    for _ in 0..30 {
        let outs = h.push(1, &speech(100));
        done = done.or(sent(&outs, 1).into_iter().find(|v| v["event"] == "done"));
    }
    let done = done.expect("session finished");
    assert_eq!(
        (done["reason"].clone(), done["audio_ms"].clone()),
        ("max_duration".into(), 100.into())
    );
    assert_eq!(h.log().samples, 1600);
}

/// Pushes `pcm` in `chunk`-sample requests; returns the session's `done` (stopping if needed).
fn run_silence(pcm: &[i16], chunk: usize) -> Value {
    let mut h = h(FakeMode::EverySecond);
    h.req(
        1,
        r#"{"op":"start","source":"push","silence_stop_ms":1000}"#,
    );
    for part in pcm.chunks(chunk) {
        if let Some(done) = sent(&h.push(1, part), 1)
            .into_iter()
            .find(|v| v["event"] == "done")
        {
            return done;
        }
    }
    one(&h.req(1, STOP), 1, "done")
}

#[test]
fn t08_silence_stop() {
    let pcm = [speech(16000), zeros(64000)].concat();
    let whole = run_silence(&pcm, pcm.len());
    let small = run_silence(&pcm, 160);
    assert_eq!(
        (whole["reason"].clone(), small["reason"].clone()),
        ("silence".into(), "silence".into())
    );
    let (a, b) = (
        whole["audio_ms"].as_i64().unwrap(),
        small["audio_ms"].as_i64().unwrap(),
    );
    assert!((a - b).abs() <= 32, "{a} vs {b}");
    assert_eq!(run_silence(&zeros(80000), 80000)["reason"], "stop");
}

#[test]
fn t18_stream_flush_segments_before_done() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, r#"{"op":"start","source":"push","stream":true}"#);
    let outs = h.push(1, &speech(24000));
    assert_eq!(one(&outs, 1, "segment")["text"], "seg1");
    let events: Vec<_> = sent(&h.req(1, STOP), 1)
        .iter()
        .map(|v| v["event"].clone())
        .collect();
    assert_eq!(events, ["ack", "segment", "done"]);
}

/// Wraps [`FakeEngine`] and sets the cancel hint to gen 1 on the `after`-th accept.
struct Hinting {
    inner: FakeEngine,
    hint: Arc<OnceLock<Arc<AtomicU64>>>,
    after: usize,
}

impl Engine for Hinting {
    fn begin(&mut self, hotwords: &[String]) {
        self.inner.begin(hotwords);
    }
    fn accept(&mut self, samples: &[f32]) -> Vec<EngineEvent> {
        self.after -= 1;
        if self.after == 0 {
            self.hint.get().unwrap().store(1, Ordering::Relaxed);
        }
        self.inner.accept(samples)
    }
    fn finish(&mut self) -> Vec<EngineEvent> {
        self.inner.finish()
    }
    fn in_speech(&self) -> bool {
        self.inner.in_speech()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

#[test]
fn t19_cancel_hint() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, PUSH);
    h.ctl.cancel_hint().store(1, Ordering::Relaxed);
    assert!(h
        .msg(AudioMsg::Samples {
            gen: 1,
            data: vec![0.5; 5120]
        })
        .is_empty());
    assert!(h.push(1, &speech(5120)).is_empty());
    assert_eq!(h.log().accepts, 0);

    let (inner, log) = FakeEngine::new(FakeMode::EverySecond);
    let hint = Arc::new(OnceLock::new());
    let engine = Hinting {
        inner,
        hint: Arc::clone(&hint),
        after: 3,
    };
    let mut h = with_engine(Box::new(engine), log);
    hint.set(h.ctl.cancel_hint()).unwrap();
    h.req(1, PUSH);
    h.push(1, &speech(5120));
    assert_eq!(h.log().accepts, 3);
    assert_eq!(
        tags(&h.req(1, CANCEL))[..2],
        ["send:1:ack", "send:1:cancelled"]
    );
}

#[test]
fn t20_accounting() {
    for (n, ms) in [(1, 0), (511, 31), (512, 32), (513, 32), (16000, 1000)] {
        let mut h = h(FakeMode::EverySecond);
        h.req(1, PUSH);
        h.push(1, &speech(n));
        assert_eq!(one(&h.req(1, STOP), 1, "done")["audio_ms"], ms, "n={n}");
        assert_eq!(h.log().samples, n);
    }
}

#[test]
fn t21_cap_single_push() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, r#"{"op":"start","source":"push","max_ms":100}"#);
    let done = one(&h.push(1, &speech(48000)), 1, "done");
    assert_eq!(
        (done["reason"].clone(), done["audio_ms"].clone()),
        ("max_duration".into(), 100.into())
    );
    assert_eq!(h.log().samples, 1600);
}

#[test]
fn t22_tail_segment() {
    let mut h = h(FakeMode::ResidualOnly);
    h.req(1, r#"{"op":"start","source":"push","stream":true}"#);
    assert!(h.push(1, &speech(1000)).is_empty());
    let outs = h.req(1, STOP);
    assert_eq!(one(&outs, 1, "segment")["text"], "seg1");
    let done = one(&outs, 1, "done");
    assert_eq!(
        (
            done["raw_text"].clone(),
            done["segments"][0]["text"].clone()
        ),
        ("seg1".into(), "seg1".into())
    );
}

#[test]
fn t23_silence_cut() {
    let mut h = h(FakeMode::EverySecond);
    h.req(
        1,
        r#"{"op":"start","source":"push","silence_stop_ms":1000}"#,
    );
    let done = one(
        &h.push(1, &[speech(16000), zeros(80000)].concat()),
        1,
        "done",
    );
    assert_eq!(done["reason"], "silence");
    // Last speech window ends at 32 × 512 = 16384; the cut lands 16000 samples later,
    // rounded up to a window: 64 × 512.
    assert_eq!(h.log().samples, 64 * 512);
    assert_eq!(done["audio_ms"], 64 * 512 * 1000 / 16000);
}

#[test]
fn t25_huge_ms_values_saturate() {
    let (engine, log) = FakeEngine::new(FakeMode::EverySecond);
    let mut cfg = cfg();
    cfg.session.max_ms = u64::MAX;
    let mut h = H {
        ctl: Controller::new(Box::new(engine), cfg),
        log,
        now: 0,
    };
    let start = format!(
        r#"{{"op":"start","source":"push","silence_stop_ms":{},"max_ms":{}}}"#,
        1u64 << 60,
        i64::MAX
    );
    h.req(1, &start);
    let outs = h.push(1, &[speech(16000), zeros(64000)].concat());
    assert!(sent(&outs, 1).iter().all(|v| v["event"] != "done"));
    assert_eq!(one(&h.req(1, STOP), 1, "done")["reason"], "stop");
}
