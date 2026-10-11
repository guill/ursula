//! Deterministic [`Engine`] for controller tests. The controller owns the boxed
//! engine, so observations go through the shared [`FakeLog`] handle.

use std::sync::{Arc, Mutex};

use super::{Engine, EngineEvent, SAMPLE_RATE, WINDOW};
use crate::protocol::Capabilities;

/// Cumulative over the engine's lifetime (not reset by `begin`); tests that
/// need per-session numbers reset it with `*log.lock().unwrap() = FakeLog::default()`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FakeLog {
    pub begins: usize,
    /// Hotwords passed to the last `begin`.
    pub hotwords: Vec<String>,
    pub accepts: usize,
    /// Real samples received (sum of `accept` slice lengths).
    pub samples: usize,
    pub finishes: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FakeMode {
    /// `Segment "seg<N>"` each time the session's samples cross N × 16000;
    /// `finish` emits one more for a leftover partial second.
    #[default]
    EverySecond,
    /// Only the residual `accept` (< 512 samples) emits a segment; `finish` emits nothing.
    ResidualOnly,
}

pub struct FakeEngine {
    mode: FakeMode,
    log: Arc<Mutex<FakeLog>>,
    /// Session samples so far.
    acc: usize,
    segs: usize,
    speech: bool,
}

impl FakeEngine {
    pub fn new(mode: FakeMode) -> (Self, Arc<Mutex<FakeLog>>) {
        let log = Arc::new(Mutex::new(FakeLog::default()));
        let engine = Self {
            mode,
            log: Arc::clone(&log),
            acc: 0,
            segs: 0,
            speech: false,
        };
        (engine, log)
    }

    fn segment(&mut self, from: usize, to: usize) -> EngineEvent {
        self.segs += 1;
        let secs = |n: usize| n as f32 / SAMPLE_RATE as f32;
        EngineEvent::Segment {
            text: format!("seg{}", self.segs),
            start: secs(from),
            end: secs(to),
        }
    }

    fn log(&self) -> std::sync::MutexGuard<'_, FakeLog> {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Engine for FakeEngine {
    fn begin(&mut self, hotwords: &[String]) {
        let mut log = self.log();
        log.begins += 1;
        log.hotwords = hotwords.to_vec();
        drop(log);
        (self.acc, self.segs, self.speech) = (0, 0, false);
    }

    fn accept(&mut self, samples: &[f32]) -> Vec<EngineEvent> {
        let mut log = self.log();
        log.accepts += 1;
        log.samples += samples.len();
        drop(log);
        self.speech = samples.iter().any(|&s| s != 0.0);
        let before = self.acc;
        self.acc += samples.len();
        match self.mode {
            FakeMode::EverySecond => (before / SAMPLE_RATE + 1..=self.acc / SAMPLE_RATE)
                .map(|n| self.segment((n - 1) * SAMPLE_RATE, n * SAMPLE_RATE))
                .collect(),
            FakeMode::ResidualOnly if samples.len() < WINDOW => {
                vec![self.segment(before, self.acc)]
            }
            FakeMode::ResidualOnly => Vec::new(),
        }
    }

    fn finish(&mut self) -> Vec<EngineEvent> {
        self.log().finishes += 1;
        let done = self.acc / SAMPLE_RATE * SAMPLE_RATE;
        match self.mode {
            FakeMode::EverySecond if self.acc > done => vec![self.segment(done, self.acc)],
            FakeMode::EverySecond | FakeMode::ResidualOnly => Vec::new(),
        }
    }

    fn in_speech(&self) -> bool {
        self.speech
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hotwords: false,
            partials: false,
        }
    }
}
