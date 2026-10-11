//! Supervision ticks: mic worker stall, open timeout, mic data stall.

use super::{error, Controller, State};
use crate::protocol::{session_id, ErrorCode, Source};

const LIMIT_MS: u64 = 2000;

impl Controller {
    pub(super) fn tick(&mut self, now: u64, last_data_ms: u64) {
        self.now = now;
        if let Some(w) = self.mic_worker.as_mut() {
            let stalled = w
                .stop_requested_ms
                .is_some_and(|t| now.saturating_sub(t) > LIMIT_MS);
            if stalled && !w.stalled_logged {
                eprintln!("warning: microphone reader did not exit; capture stalled");
                w.stalled_logged = true;
            }
        }
        let timeout = match &self.state {
            State::Opening(s) if now.saturating_sub(s.started_ms) > LIMIT_MS => {
                "microphone did not open within 2 s"
            }
            State::Recording(s)
                if s.source == Source::Mic
                    && now.saturating_sub(last_data_ms.max(s.opened_ms)) > LIMIT_MS =>
            {
                "no audio for 2 s"
            }
            _ => return,
        };
        if let State::Opening(s) | State::Recording(s) = std::mem::take(&mut self.state) {
            let sid = Some(session_id(s.gen));
            self.end(s, Some(error(ErrorCode::AudioUnavailable, timeout, sid)));
        }
    }
}
