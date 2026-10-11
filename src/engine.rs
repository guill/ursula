//! Speech engine seam (DESIGN.md "Architecture"): the session controller feeds
//! 16 kHz mono f32 audio and consumes [`EngineEvent`]s.

use crate::protocol::Capabilities;

pub mod vad_offline;

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;

/// VAD window and streaming step, in samples.
pub const WINDOW: usize = 512;
pub const SAMPLE_RATE: usize = 16_000;

#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// Final, never-revised text; `start`/`end` in seconds from session start.
    Segment { text: String, start: f32, end: f32 },
    /// Reserved for a future streaming engine; never emitted in v1.
    #[expect(dead_code, reason = "no v1 engine emits partials")]
    Partial { text: String },
}

/// One engine instance serves one session at a time and is owned by one thread.
pub trait Engine: Send {
    /// Starts a new session; `hotwords` are the request's words.
    fn begin(&mut self, hotwords: &[String]);
    /// Exactly [`WINDOW`] samples while streaming; fewer means the final tail
    /// (zero-padded internally, only real samples count).
    fn accept(&mut self, samples: &[f32]) -> Vec<EngineEvent>;
    /// Flushes buffered speech and decodes it.
    fn finish(&mut self) -> Vec<EngineEvent>;
    /// Whether the most recently fed window was flagged as speech.
    fn in_speech(&self) -> bool;
    fn capabilities(&self) -> Capabilities;
}

/// Appends `chunk` to `residual` and returns every full window; the remainder
/// (< [`WINDOW`] samples) stays in `residual`.
pub fn steps512(residual: &mut Vec<f32>, chunk: &[f32]) -> Vec<[f32; WINDOW]> {
    residual.extend_from_slice(chunk);
    let windows = residual.as_chunks::<WINDOW>().0.to_vec();
    residual.drain(..windows.len() * WINDOW);
    windows
}

/// Contiguous `[start, end)` pieces covering `[0, len)`, each at most `max` long.
/// `max` must be > 0.
pub fn slice_pcm(len: usize, max: usize) -> Vec<(usize, usize)> {
    (0..len)
        .step_by(max)
        .map(|a| (a, (a + max).min(len)))
        .collect()
}
