//! NDJSON wire protocol: one JSON object per line.
//! Requests go client → daemon, events daemon → client. Field names are the
//! wire names from DESIGN.md "API" plus the normative plan clarifications.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Max request line length in bytes, excluding the trailing `\n`.
pub const MAX_LINE: usize = 1024 * 1024;

/// One request line: the op plus an optional `id`, echoed verbatim on replies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(flatten)]
    pub req: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Start {
        source: Source,
        /// Emit `segment` events; otherwise only `done`.
        #[serde(default)]
        stream: bool,
        #[serde(default)]
        hotwords: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        /// Post-processor names from config, run in order.
        #[serde(default)]
        postprocess: Vec<String>,
        /// Signed so the controller can answer `<= 0` with `bad_request`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        silence_stop_ms: Option<u64>,
    },
    /// Base64 16 kHz mono s16le PCM; push sessions only.
    Audio {
        data: String,
    },
    /// `session`, when given, must match the active session.
    Stop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// `session`, when given, must match the active session.
    Cancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    Status,
}

/// Session audio source: `"mic"`, `"push"` or `{"file": "/abs/path.wav"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SourceRepr", into = "SourceRepr")]
pub enum Source {
    Mic,
    Push,
    /// Always an absolute path; relative paths are rejected while parsing.
    File(String),
}

#[derive(Serialize, Deserialize)]
#[serde(
    untagged,
    expecting = r#"source must be "mic", "push" or {"file": "/abs/path.wav"}"#
)]
enum SourceRepr {
    Named(SourceName),
    File { file: String },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SourceName {
    Mic,
    Push,
}

impl TryFrom<SourceRepr> for Source {
    type Error = String;

    fn try_from(repr: SourceRepr) -> Result<Self, String> {
        match repr {
            SourceRepr::Named(SourceName::Mic) => Ok(Self::Mic),
            SourceRepr::Named(SourceName::Push) => Ok(Self::Push),
            SourceRepr::File { file } if Path::new(&file).is_absolute() => Ok(Self::File(file)),
            SourceRepr::File { file } => Err(format!("file path must be absolute: {file}")),
        }
    }
}

impl From<Source> for SourceRepr {
    fn from(source: Source) -> Self {
        match source {
            Source::Mic => Self::Named(SourceName::Mic),
            Source::Push => Self::Named(SourceName::Push),
            Source::File(file) => Self::File { file },
        }
    }
}

/// A daemon event plus the echoed request `id` (omitted when `None`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Out {
    #[serde(flatten)]
    pub event: Event,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Started {
        session: String,
        model: String,
    },
    /// `start`/`end` in seconds. Never revised once sent.
    Segment {
        session: String,
        text: String,
        start: f32,
        end: f32,
    },
    /// Reserved: never emitted in v1.
    Partial {
        session: String,
        text: String,
    },
    Done {
        session: String,
        /// After post-processing; the authoritative result.
        text: String,
        /// Before post-processing.
        raw_text: String,
        segments: Vec<Segment>,
        audio_ms: u64,
        reason: Reason,
        warnings: Vec<String>,
    },
    Cancelled {
        session: String,
    },
    Status {
        model: String,
        busy: bool,
        version: String,
        capabilities: Capabilities,
        /// The active session id; serialized as `null` when idle.
        session: Option<String>,
    },
    /// `session` is set for in-session (terminal) errors and for `busy`.
    Error {
        code: ErrorCode,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Reply to `stop`/`cancel`, sent to the requesting connection.
    Ack {
        op: AckOp,
        session: String,
    },
}

/// One decoded segment; `start`/`end` in seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub start: f32,
    pub end: f32,
}

/// Why a session finished (`done.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Stop,
    Silence,
    MaxDuration,
    EndOfFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Busy,
    NoSession,
    BadRequest,
    BadAudio,
    AudioUnavailable,
    UnknownPostprocessor,
}

/// The op an `ack` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckOp {
    Stop,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub hotwords: bool,
    pub partials: bool,
}

/// Parses one request line (a trailing `\n` is fine).
///
/// # Errors
/// Returns the request's `id` (when the line is a JSON object carrying one,
/// even if the op is invalid) and a message for a `bad_request` reply.
pub fn parse_line(line: &str) -> Result<Envelope, (Option<Value>, String)> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| (None, format!("invalid JSON: {e}")))?;
    let id = value.get("id").filter(|id| !id.is_null()).cloned();
    serde_json::from_value(value).map_err(|e| (id, e.to_string()))
}

/// Serializes an event as one NDJSON line, `\n` included.
#[must_use]
pub fn to_line(out: &Out) -> String {
    // Infallible: derived impls over string-keyed maps; every variant's shape
    // is exercised by the tests.
    let mut line = serde_json::to_string(out).expect("protocol events always serialize");
    line.push('\n');
    line
}

/// Wire id of session generation `gen` (gens start at 1).
#[must_use]
pub fn session_id(gen: u64) -> String {
    format!("s-{gen}")
}

/// Inverse of [`session_id`]; `None` for anything it would not produce,
/// including `s-0` (no such generation).
#[must_use]
pub fn parse_session_id(id: &str) -> Option<u64> {
    let gen = id.strip_prefix("s-")?.parse().ok()?;
    (gen != 0 && session_id(gen) == id).then_some(gen)
}

#[cfg(test)]
mod tests;
