//! Controller tests over [`FakeEngine`]: drive `handle` with explicit times and
//! inspect the returned outputs as wire JSON.

use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value;

use super::{ConnId, Controller, Input, Output};
use crate::audio::AudioMsg;
use crate::config::{Config, Postprocessor};
use crate::engine::fake::{FakeEngine, FakeLog, FakeMode};
use crate::engine::Engine;
use crate::protocol::parse_line;

mod audio;
mod basic;
mod mic;

pub struct H {
    pub ctl: Controller,
    pub log: Arc<Mutex<FakeLog>>,
    pub now: u64,
}

pub fn cfg() -> Config {
    let mut cfg = Config::default();
    cfg.model.name = "fake".into();
    let p = Postprocessor {
        command: vec!["true".into()],
        timeout_ms: 1000,
    };
    cfg.postprocessors.insert("p".into(), p);
    cfg
}

pub fn h(mode: FakeMode) -> H {
    let (engine, log) = FakeEngine::new(mode);
    with_engine(Box::new(engine), log)
}

pub fn with_engine(engine: Box<dyn Engine>, log: Arc<Mutex<FakeLog>>) -> H {
    H {
        ctl: Controller::new(engine, cfg()),
        log,
        now: 0,
    }
}

impl H {
    pub fn input(&mut self, input: Input) -> Vec<Output> {
        self.ctl.handle(input, self.now)
    }

    pub fn req(&mut self, conn: ConnId, line: &str) -> Vec<Output> {
        let env = parse_line(line).expect("test request parses");
        self.input(Input::Request { conn, env })
    }

    /// Pushes s16 samples as one `audio` request.
    pub fn push(&mut self, conn: ConnId, pcm: &[i16]) -> Vec<Output> {
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        let line = format!(r#"{{"op":"audio","data":"{}"}}"#, STANDARD.encode(bytes));
        self.req(conn, &line)
    }

    pub fn msg(&mut self, msg: AudioMsg) -> Vec<Output> {
        self.input(Input::Audio(msg))
    }

    pub fn tick(&mut self, now_ms: u64, last_data_ms: u64) -> Vec<Output> {
        self.now = now_ms;
        self.input(Input::Tick {
            now_ms,
            last_data_ms,
        })
    }

    pub fn log(&self) -> FakeLog {
        self.log.lock().unwrap().clone()
    }

    pub fn busy(&self) -> bool {
        self.ctl.snapshot().busy
    }
}

/// Short tag per output: `"send:<conn>:<event>"`, `"close:<conn>"`, `"open_mic"`, ...
pub fn tags(outs: &[Output]) -> Vec<String> {
    outs.iter()
        .map(|o| match o {
            Output::Send { conn, out } => {
                format!("send:{conn}:{}", json(out)["event"].as_str().unwrap())
            }
            Output::CloseConn { conn } => format!("close:{conn}"),
            Output::OpenMic { gen } => format!("open_mic:{gen}"),
            Output::StartFile { gen, .. } => format!("start_file:{gen}"),
            Output::StopCapture { gen } => format!("stop_capture:{gen}"),
            Output::RunPost { gen, .. } => format!("run_post:{gen}"),
            Output::CancelPost { gen } => format!("cancel_post:{gen}"),
            Output::Publish(_) => "publish".into(),
        })
        .collect()
}

fn json(out: &crate::protocol::Out) -> Value {
    serde_json::to_value(out).unwrap()
}

/// Wire JSON of every event sent to `conn`, in order.
pub fn sent(outs: &[Output], conn: ConnId) -> Vec<Value> {
    outs.iter()
        .filter_map(|o| match o {
            Output::Send { conn: c, out } if *c == conn => Some(json(out)),
            _ => None,
        })
        .collect()
}

/// The single event sent to `conn` named `event`.
pub fn one(outs: &[Output], conn: ConnId, event: &str) -> Value {
    let found: Vec<Value> = sent(outs, conn)
        .into_iter()
        .filter(|v| v["event"] == event)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "want one {event} to {conn}: {:?}",
        tags(outs)
    );
    found.into_iter().next().unwrap()
}

pub fn speech(n: usize) -> Vec<i16> {
    vec![1000; n]
}

pub fn zeros(n: usize) -> Vec<i16> {
    vec![0; n]
}

pub const PUSH: &str = r#"{"op":"start","source":"push"}"#;
pub const MIC: &str = r#"{"op":"start","source":"mic"}"#;
pub const STOP: &str = r#"{"op":"stop"}"#;
pub const CANCEL: &str = r#"{"op":"cancel"}"#;
