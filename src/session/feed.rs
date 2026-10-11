//! Audio-source messages, admission, 512-sample stepping, auto-stop and the final flush.

use super::{error, failure, Controller, Output, State, S};
use crate::audio::AudioMsg;
use crate::engine::{steps512, EngineEvent, SAMPLE_RATE, WINDOW};
use crate::postprocess::PostJob;
use crate::protocol::{session_id, Event, Reason, Segment};

impl Controller {
    pub(super) fn audio(&mut self, msg: AudioMsg) {
        let gen = match &msg {
            AudioMsg::MicExited { gen } => {
                if self.mic_worker.as_ref().is_some_and(|w| w.gen == *gen) {
                    self.mic_worker = None;
                }
                return;
            }
            AudioMsg::Opened { gen }
            | AudioMsg::Samples { gen, .. }
            | AudioMsg::Eof { gen }
            | AudioMsg::Failed { gen, .. } => *gen,
        };
        if self.state.session().is_none_or(|s| s.gen != gen) {
            return;
        }
        match (std::mem::take(&mut self.state), msg) {
            (State::Opening(mut s), AudioMsg::Opened { .. }) => {
                s.opened_ms = self.now;
                self.started(&s);
                self.state = State::Recording(s);
                self.publish();
            }
            (State::Recording(s), AudioMsg::Samples { data, .. }) => self.feed(s, &data),
            (State::Recording(s), AudioMsg::Eof { .. }) => self.finish(s, Reason::EndOfFile),
            (State::Opening(s) | State::Recording(s), AudioMsg::Failed { msg, .. }) => {
                let (code, message) = failure(&msg);
                let sid = Some(session_id(s.gen));
                self.end(s, Some(error(code, message, sid)));
            }
            (state, _) => self.state = state,
        }
    }

    /// Feeds a Recording session; leaves it Recording or finishes it.
    pub(super) fn feed(&mut self, mut s: S, samples: &[f32]) {
        if self.hinted(s.gen) {
            self.state = State::Recording(s);
            return;
        }
        let room = usize::try_from(s.max_samples - s.accepted).unwrap_or(usize::MAX);
        let kept = &samples[..samples.len().min(room)];
        s.accepted += kept.len() as u64;
        for step in steps512(&mut self.residual, kept) {
            if self.hinted(s.gen) {
                // A queued cancel ends the session; never decode its audio.
                self.residual.clear();
                self.state = State::Recording(s);
                return;
            }
            let events = self.engine.accept(&step);
            s.processed += WINDOW as u64;
            self.emit(&mut s, events);
            if self.engine.in_speech() {
                s.last_voice_at = s.processed;
                s.seen_speech = true;
            }
            let silent = |n| s.seen_speech && s.processed - s.last_voice_at >= n;
            if s.silence_stop_samples.is_some_and(silent) {
                // Audio after the cut is discarded, never decoded nor counted.
                s.accepted = s.processed;
                self.residual.clear();
                return self.finish(s, Reason::Silence);
            }
        }
        if s.accepted >= s.max_samples {
            return self.finish(s, Reason::MaxDuration);
        }
        self.state = State::Recording(s);
    }

    /// Stores segments and streams them to the owner when `stream`.
    fn emit(&mut self, s: &mut S, events: Vec<EngineEvent>) {
        for event in events {
            let EngineEvent::Segment { text, start, end } = event else {
                continue; // partials: never emitted in v1
            };
            if s.stream {
                let seg = Event::Segment {
                    session: session_id(s.gen),
                    text: text.clone(),
                    start,
                    end,
                };
                self.send_owner(s, seg);
            }
            s.segments.push(Segment { text, start, end });
        }
    }

    /// Stops capture, flushes the engine, then sends `done` or starts post-processing.
    pub(super) fn finish(&mut self, mut s: S, reason: Reason) {
        self.stop_capture(&s);
        s.reason = Some(reason);
        let mut events = Vec::new();
        if !self.residual.is_empty() {
            let tail = std::mem::take(&mut self.residual);
            events = self.engine.accept(&tail);
        }
        events.extend(self.engine.finish());
        self.emit(&mut s, events);
        let raw = raw_text(&s.segments);
        if s.post_names.is_empty() {
            let done = self.done(&s, raw, Vec::new());
            return self.close(s, Some(done));
        }
        let chain = s
            .post_names
            .iter()
            .filter_map(|n| Some((n.clone(), self.cfg.postprocessors.get(n)?.clone())))
            .collect();
        let job = PostJob {
            text: raw,
            context: s.context.clone(),
            hotwords: s.hotwords.clone(),
            segments: s.segments.clone(),
            chain,
        };
        self.outs.push(Output::RunPost { gen: s.gen, job });
        self.state = State::Finishing(s);
        self.publish();
    }

    pub(super) fn done(&self, s: &S, text: String, post_warnings: Vec<String>) -> Event {
        let mut warnings = s.warnings.clone();
        warnings.extend(post_warnings);
        Event::Done {
            session: session_id(s.gen),
            text,
            raw_text: raw_text(&s.segments),
            segments: s.segments.clone(),
            audio_ms: s.accepted * 1000 / SAMPLE_RATE as u64,
            reason: s.reason.unwrap_or(Reason::Stop), // always set by `finish`
            warnings,
        }
    }
}

fn raw_text(segments: &[Segment]) -> String {
    let texts: Vec<&str> = segments
        .iter()
        .map(|s| s.text.trim())
        .filter(|t| !t.is_empty())
        .collect();
    texts.join(" ")
}
