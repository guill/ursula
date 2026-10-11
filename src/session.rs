//! Single-session controller (DESIGN.md "Sessions"; plan todo 9 transition table).
//! `handle` is pure apart from the WAV header read and engine inference: every side
//! effect is returned as an [`Output`] for the server runtime to execute in order.

use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use hound::WavReader;
use serde_json::Value;

use crate::audio::AudioMsg;
use crate::config::Config;
use crate::engine::Engine;
use crate::postprocess::PostJob;
use crate::protocol::{
    session_id, Capabilities, Envelope, ErrorCode, Event, Out, Reason, Segment, Source,
};

mod feed;
mod handle;
mod tick;

#[cfg(test)]
mod tests;

pub type ConnId = u64;

/// Controller input; also the ingress-channel message type of the server.
pub enum Input {
    Request {
        conn: ConnId,
        env: Envelope,
    },
    Audio(AudioMsg),
    /// Half-close: the connection sends no more requests.
    ReadClosed(ConnId),
    /// Peer gone, write error or slow reader.
    ConnClosed(ConnId),
    PostDone {
        gen: u64,
        text: String,
        warnings: Vec<String>,
    },
    Tick {
        now_ms: u64,
        last_data_ms: u64,
    },
}

/// Side effects for the server runtime, executed in order.
pub enum Output {
    Send {
        conn: ConnId,
        out: Out,
    },
    /// Flush pending writes, then close.
    CloseConn {
        conn: ConnId,
    },
    OpenMic {
        gen: u64,
    },
    StartFile {
        gen: u64,
        reader: WavReader<BufReader<File>>,
    },
    StopCapture {
        gen: u64,
    },
    RunPost {
        gen: u64,
        job: PostJob,
    },
    CancelPost {
        gen: u64,
    },
    Publish(Snapshot),
}

/// What reader threads need to answer `status` and set the cancel hint.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub busy: bool,
    pub session: Option<String>,
    /// 0 when idle.
    pub active_gen: u64,
    pub owner: Option<ConnId>,
    pub model: String,
    pub version: String,
    pub capabilities: Capabilities,
}

struct S {
    gen: u64,
    owner: ConnId,
    start_id: Option<Value>,
    source: Source,
    stream: bool,
    /// Config ∪ request words, for post-processors.
    hotwords: Vec<String>,
    context: Option<String>,
    post_names: Vec<String>,
    started_ms: u64,
    max_samples: u64,
    silence_stop_samples: Option<u64>,
    /// Real samples admitted (including the unprocessed residual).
    accepted: u64,
    /// Samples fed as full windows.
    processed: u64,
    last_voice_at: u64,
    seen_speech: bool,
    segments: Vec<Segment>,
    warnings: Vec<String>,
    /// Set by `finish`.
    reason: Option<Reason>,
    opened_ms: u64,
}

#[derive(Default)]
enum State {
    #[default]
    Idle,
    /// Mic only, before the stream is ready.
    Opening(S),
    Recording(S),
    /// Post-processors running.
    Finishing(S),
}

impl State {
    fn session(&self) -> Option<&S> {
        match self {
            Self::Idle => None,
            Self::Opening(s) | Self::Recording(s) | Self::Finishing(s) => Some(s),
        }
    }
}

struct MicWorker {
    gen: u64,
    stop_requested_ms: Option<u64>,
    stalled_logged: bool,
}

pub struct Controller {
    engine: Box<dyn Engine>,
    cfg: Config,
    state: State,
    gen: u64,
    mic_worker: Option<MicWorker>,
    residual: Vec<f32>,
    half_closed: HashSet<ConnId>,
    cancel_hint_gen: Arc<AtomicU64>,
    /// Time of the input being handled.
    now: u64,
    /// Outputs of the input being handled.
    outs: Vec<Output>,
}

impl Controller {
    pub fn new(engine: Box<dyn Engine>, cfg: Config) -> Self {
        Self {
            engine,
            cfg,
            state: State::Idle,
            gen: 0,
            mic_worker: None,
            residual: Vec::new(),
            half_closed: HashSet::new(),
            cancel_hint_gen: Arc::new(AtomicU64::new(0)),
            now: 0,
            outs: Vec::new(),
        }
    }

    /// Shared with reader threads: storing a gen there makes the controller discard
    /// that gen's queued audio without decoding. Cleared (0) whenever a session ends.
    pub fn cancel_hint(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.cancel_hint_gen)
    }

    pub fn snapshot(&self) -> Snapshot {
        let s = self.state.session();
        Snapshot {
            busy: s.is_some(),
            session: s.map(|s| session_id(s.gen)),
            active_gen: s.map_or(0, |s| s.gen),
            owner: s.map(|s| s.owner),
            model: self.cfg.model.name.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities: self.engine.capabilities(),
        }
    }

    pub fn handle(&mut self, input: Input, now_ms: u64) -> Vec<Output> {
        self.now = now_ms;
        match input {
            Input::Request { conn, env } => self.request(conn, env),
            Input::Audio(msg) => self.audio(msg),
            Input::ReadClosed(conn) => self.read_closed(conn),
            Input::ConnClosed(conn) => self.conn_closed(conn),
            Input::PostDone {
                gen,
                text,
                warnings,
            } => self.post_done(gen, text, warnings),
            Input::Tick {
                now_ms,
                last_data_ms,
            } => self.tick(now_ms, last_data_ms),
        }
        std::mem::take(&mut self.outs)
    }

    fn send(&mut self, conn: ConnId, id: Option<Value>, event: Event) {
        let out = Out { event, id };
        self.outs.push(Output::Send { conn, out });
    }

    /// Session events go to the owner and carry the `start` request's id.
    fn send_owner(&mut self, s: &S, event: Event) {
        self.send(s.owner, s.start_id.clone(), event);
    }

    fn publish(&mut self) {
        let snap = self.snapshot();
        self.outs.push(Output::Publish(snap));
    }

    fn hinted(&self, gen: u64) -> bool {
        self.cancel_hint_gen.load(Ordering::Relaxed) == gen
    }

    fn stop_capture(&mut self, s: &S) {
        match s.source {
            Source::Push => return,
            Source::Mic => {
                if let Some(w) = self.mic_worker.as_mut().filter(|w| w.gen == s.gen) {
                    w.stop_requested_ms.get_or_insert(self.now);
                }
            }
            Source::File(_) => {}
        }
        self.outs.push(Output::StopCapture { gen: s.gen });
    }

    /// Sends the terminal event, stops capture, then [`Self::close`].
    fn end(&mut self, s: S, terminal: Option<Event>) {
        if let Some(event) = terminal {
            self.send_owner(&s, event);
        }
        self.stop_capture(&s);
        self.close(s, None);
    }

    /// Sends the terminal event (`None` = silent) and goes Idle.
    fn close(&mut self, s: S, terminal: Option<Event>) {
        if let Some(event) = terminal {
            self.send_owner(&s, event);
        }
        self.state = State::Idle;
        self.cancel_hint_gen.store(0, Ordering::Relaxed);
        self.publish();
        if self.half_closed.remove(&s.owner) {
            self.outs.push(Output::CloseConn { conn: s.owner });
        }
    }
}

fn error(code: ErrorCode, message: impl Into<String>, session: Option<String>) -> Event {
    Event::Error {
        code,
        message: message.into(),
        session,
    }
}

/// Splits an audio-source failure `"<code>: <message>"` into its code and message.
fn failure(msg: &str) -> (ErrorCode, String) {
    let codes = [
        ("bad_audio:", ErrorCode::BadAudio),
        ("audio_unavailable:", ErrorCode::AudioUnavailable),
    ];
    codes
        .iter()
        .find_map(|&(prefix, code)| Some((code, msg.strip_prefix(prefix)?.trim().to_owned())))
        .unwrap_or((ErrorCode::AudioUnavailable, msg.to_owned()))
}
