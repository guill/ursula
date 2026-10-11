//! Request, audio-message and connection dispatch.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use hound::WavReader;

use serde_json::Value;

use super::{error, failure, ConnId, Controller, MicWorker, Output, State, S};
use crate::audio;
use crate::engine::SAMPLE_RATE;
use crate::protocol::{session_id, AckOp, Envelope, ErrorCode, Event, Reason, Request, Source};

const MIC_BUSY: &str =
    "previous microphone capture has not shut down (stalled?); retry shortly or restart ursula";
type Wav = WavReader<BufReader<File>>;
const SAMPLES_PER_MS: u64 = SAMPLE_RATE as u64 / 1000;

impl Controller {
    pub(super) fn request(&mut self, conn: ConnId, env: Envelope) {
        let Envelope { id, req } = env;
        match req {
            Request::Start { .. } => match self.state.session() {
                Some(s) => {
                    let sid = session_id(s.gen);
                    let busy = error(
                        ErrorCode::Busy,
                        format!("session {sid} is active"),
                        Some(sid),
                    );
                    self.send(conn, id, busy);
                }
                None => self.start(conn, id, req),
            },
            Request::Stop { session } => self.stop_or_cancel(conn, id, (AckOp::Stop, session)),
            Request::Cancel { session } => self.stop_or_cancel(conn, id, (AckOp::Cancel, session)),
            Request::Audio { data } => self.push_audio(conn, id, &data),
            Request::Status => {
                let snap = self.snapshot();
                let status = Event::Status {
                    model: snap.model,
                    busy: snap.busy,
                    version: snap.version,
                    capabilities: snap.capabilities,
                    session: snap.session,
                };
                self.send(conn, id, status);
            }
        }
    }

    fn start(&mut self, conn: ConnId, id: Option<Value>, req: Request) {
        let Request::Start {
            source,
            stream,
            hotwords,
            context,
            postprocess,
            max_ms,
            silence_stop_ms,
        } = req
        else {
            return;
        };
        let reader = match self.validate(&source, &postprocess, max_ms) {
            Ok(reader) => reader,
            Err((code, msg)) => return self.send(conn, id, error(code, msg, None)),
        };

        self.gen += 1;
        let gen = self.gen;
        self.engine.begin(&hotwords);
        self.residual.clear();
        let mut words = self.cfg.hotwords.words.clone();
        for w in hotwords {
            if !words.contains(&w) {
                words.push(w);
            }
        }
        let mut warnings = Vec::new();
        if !words.is_empty() && !self.engine.capabilities().hotwords {
            warnings.push("hotwords ignored: not enabled for this model".to_owned());
        }
        let cap_ms = max_ms.map_or(self.cfg.session.max_ms, |m| {
            self.cfg.session.max_ms.min(m.unsigned_abs())
        });
        let s = S {
            gen,
            owner: conn,
            start_id: id,
            source,
            stream,
            hotwords: words,
            context,
            post_names: postprocess,
            started_ms: self.now,
            max_samples: cap_ms * SAMPLES_PER_MS,
            silence_stop_samples: silence_stop_ms.map(|ms| ms * SAMPLES_PER_MS),
            accepted: 0,
            processed: 0,
            last_voice_at: 0,
            seen_speech: false,
            segments: Vec::new(),
            warnings,
            reason: None,
            opened_ms: self.now,
        };
        if s.source == Source::Mic {
            self.mic_worker = Some(MicWorker {
                gen,
                stop_requested_ms: None,
                stalled_logged: false,
            });
            self.outs.push(Output::OpenMic { gen });
            self.state = State::Opening(s);
        } else {
            self.started(&s);
            if let Some(reader) = reader {
                self.outs.push(Output::StartFile { gen, reader });
            }
            self.state = State::Recording(s);
        }
        self.publish();
    }

    /// New-session checks; on failure no session is created.
    fn validate(
        &self,
        source: &Source,
        post: &[String],
        max_ms: Option<i64>,
    ) -> Result<Option<Wav>, (ErrorCode, String)> {
        let reject = |code, msg: String| Err((code, msg));
        if let Some(name) = post
            .iter()
            .find(|n| !self.cfg.postprocessors.contains_key(*n))
        {
            return reject(
                ErrorCode::UnknownPostprocessor,
                format!("unknown postprocessor: {name}"),
            );
        }
        if max_ms.is_some_and(|m| m <= 0) {
            return reject(ErrorCode::BadRequest, "max_ms must be > 0".into());
        }
        match source {
            Source::File(path) => audio::open_wav(Path::new(path))
                .map(Some)
                .map_err(|msg| (ErrorCode::BadAudio, failure(&msg).1)),
            Source::Mic if self.mic_worker.is_some() => {
                reject(ErrorCode::AudioUnavailable, MIC_BUSY.into())
            }
            Source::Mic | Source::Push => Ok(None),
        }
    }

    pub(super) fn started(&mut self, s: &S) {
        let started = Event::Started {
            session: session_id(s.gen),
            model: self.cfg.model.name.clone(),
        };
        self.send_owner(s, started);
    }

    fn stop_or_cancel(
        &mut self,
        conn: ConnId,
        id: Option<Value>,
        (op, target): (AckOp, Option<String>),
    ) {
        let active = self.state.session().map(|s| session_id(s.gen));
        let Some(sid) = active.filter(|a| target.as_ref().is_none_or(|t| t == a)) else {
            let msg = target.map_or("no active session".to_owned(), |t| {
                format!("session {t} is not active")
            });
            return self.send(conn, id, error(ErrorCode::NoSession, msg, None));
        };
        self.send(
            conn,
            id,
            Event::Ack {
                op,
                session: sid.clone(),
            },
        );
        let cancelled = Some(Event::Cancelled { session: sid });
        match (std::mem::take(&mut self.state), op) {
            (State::Opening(s), _) | (State::Recording(s), AckOp::Cancel) => self.end(s, cancelled),
            (State::Recording(s), AckOp::Stop) => self.finish(s, Reason::Stop),
            (State::Finishing(s), AckOp::Cancel) => {
                self.outs.push(Output::CancelPost { gen: s.gen });
                self.close(s, cancelled);
            }
            (state, _) => self.state = state,
        }
    }

    fn push_audio(&mut self, conn: ConnId, id: Option<Value>, data: &str) {
        let Some(s) = self.state.session() else {
            return self.send(
                conn,
                id,
                error(ErrorCode::NoSession, "no active session", None),
            );
        };
        if s.owner != conn || s.source != Source::Push {
            let msg = "audio is only accepted from the owner of a push session";
            return self.send(conn, id, error(ErrorCode::BadRequest, msg, None));
        }
        match std::mem::take(&mut self.state) {
            State::Recording(s) => match audio::decode_push(data) {
                Ok(samples) => self.feed(s, &samples),
                Err(msg) => {
                    let sid = Some(session_id(s.gen));
                    self.end(s, Some(error(ErrorCode::BadAudio, failure(&msg).1, sid)));
                }
            },
            state => {
                self.state = state;
                let msg = "session is finishing; stop was already applied";
                self.send(conn, id, error(ErrorCode::NoSession, msg, None));
            }
        }
    }

    pub(super) fn read_closed(&mut self, conn: ConnId) {
        if self.state.session().is_some_and(|s| s.owner == conn) {
            self.half_closed.insert(conn);
        } else {
            self.outs.push(Output::CloseConn { conn });
        }
    }

    pub(super) fn conn_closed(&mut self, conn: ConnId) {
        self.half_closed.remove(&conn);
        if self.state.session().is_none_or(|s| s.owner != conn) {
            return;
        }
        match std::mem::take(&mut self.state) {
            State::Idle => {}
            State::Opening(s) | State::Recording(s) => {
                eprintln!(
                    "info: session {} cancelled (connection closed)",
                    session_id(s.gen)
                );
                self.end(s, None);
            }
            State::Finishing(s) => {
                eprintln!(
                    "info: session {} cancelled (connection closed)",
                    session_id(s.gen)
                );
                self.outs.push(Output::CancelPost { gen: s.gen });
                self.close(s, None);
            }
        }
    }

    pub(super) fn post_done(&mut self, gen: u64, text: String, warnings: Vec<String>) {
        match std::mem::take(&mut self.state) {
            State::Finishing(s) if s.gen == gen => {
                let done = self.done(&s, text, warnings);
                self.close(s, Some(done));
            }
            state => self.state = state,
        }
    }
}
