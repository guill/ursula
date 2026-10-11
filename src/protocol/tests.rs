use serde_json::{json, Value};

use super::*;

fn value(json: &str) -> Value {
    serde_json::from_str(json).unwrap()
}

/// Parses a request line and re-serializes it as a `Value`.
fn request_roundtrip(json: &str) -> Value {
    serde_json::to_value(parse_line(json).unwrap()).unwrap()
}

/// Parses an event line and re-serializes it through `to_line`.
fn event_roundtrip(json: &str) -> Value {
    let out: Out = serde_json::from_str(json).unwrap();
    value(&to_line(&out))
}

// DESIGN.md:107-114, comments removed.
const START: &str = r#"{"op": "start",
 "source": "mic",
 "stream": true,
 "hotwords": ["NixOS", "sherpa-onnx"],
 "context": "Editing a Rust file about audio capture.",
 "postprocess": ["cleanup"],
 "max_ms": 120000,
 "silence_stop_ms": 1500}"#;

#[test]
fn design_request_examples_roundtrip() {
    for json in [
        START,
        r#"{"op": "audio", "data": "<base64>"}"#,
        r#"{"op": "stop"}"#,
        r#"{"op": "cancel"}"#,
        r#"{"op": "status"}"#,
    ] {
        assert_eq!(request_roundtrip(json), value(json), "{json}");
    }
}

#[test]
fn optional_request_fields_and_id_roundtrip() {
    for json in [
        r#"{"op": "start", "source": {"file": "/abs/path.wav"}, "stream": false,
            "hotwords": [], "postprocess": [], "id": {"k": [1, 2.5]}}"#,
        r#"{"op": "start", "source": "push", "stream": false, "hotwords": [],
            "postprocess": [], "id": 3}"#,
        r#"{"op": "stop", "session": "s-3", "id": "k"}"#,
        r#"{"op": "cancel", "session": "s-4", "id": 1.5}"#,
    ] {
        assert_eq!(request_roundtrip(json), value(json), "{json}");
    }
}

#[test]
fn start_fields_default_when_absent() {
    let env = parse_line(r#"{"op":"start","source":"mic"}"#).unwrap();
    let expected = Request::Start {
        source: Source::Mic,
        stream: false,
        hotwords: vec![],
        context: None,
        postprocess: vec![],
        max_ms: None,
        silence_stop_ms: None,
    };
    assert_eq!(
        env,
        Envelope {
            id: None,
            req: expected
        }
    );
}

#[test]
fn parse_line_accepts_trailing_newline() {
    let env = parse_line("{\"op\":\"status\",\"id\":2}\n").unwrap();
    assert_eq!(
        env,
        Envelope {
            id: Some(json!(2)),
            req: Request::Status
        }
    );
}

#[test]
fn relative_file_path_is_rejected_with_id() {
    let (id, msg) = parse_line(r#"{"op":"start","source":{"file":"rel.wav"},"id":3}"#).unwrap_err();
    assert_eq!(id, Some(json!(3)));
    assert!(msg.contains("absolute"), "{msg}");
}

#[test]
fn unknown_source_is_rejected() {
    let (id, _) = parse_line(r#"{"op":"start","source":"speaker","id":"a"}"#).unwrap_err();
    assert_eq!(id, Some(json!("a")));
}

#[test]
fn unknown_op_is_rejected_with_id() {
    let (id, msg) = parse_line(r#"{"op":"nope","id":"abc"}"#).unwrap_err();
    assert_eq!(id, Some(json!("abc")));
    assert!(msg.contains("nope"), "{msg}");
}

#[test]
fn missing_op_is_rejected_with_id() {
    let (id, _) = parse_line(r#"{"id":[1]}"#).unwrap_err();
    assert_eq!(id, Some(json!([1])));
}

#[test]
fn invalid_json_is_rejected_without_id() {
    assert_eq!(parse_line(r#"{"op":"stop","id":5"#).unwrap_err().0, None);
    assert_eq!(parse_line("[1]").unwrap_err().0, None);
}

#[test]
fn design_event_examples_roundtrip() {
    for json in [
        r#"{"event": "started", "session": "s-17", "model": "parakeet-tdt-0.6b-v2"}"#,
        r#"{"event": "segment", "session": "s-17", "text": "Open the config file", "start": 0.42, "end": 2.10}"#,
        r#"{"event": "partial", "session": "s-17", "text": "..."}"#,
        r#"{"event": "done", "session": "s-17",
            "text": "Open the config file.",
            "raw_text": "Open the config file",
            "segments": [{"text": "...", "start": 0.42, "end": 2.10}],
            "audio_ms": 2600,
            "reason": "stop",
            "warnings": ["hotwords ignored: not enabled for this model"]}"#,
        r#"{"event": "cancelled", "session": "s-17"}"#,
        r#"{"event": "error", "code": "busy", "message": "a session is already active"}"#,
    ] {
        assert_eq!(event_roundtrip(json), value(json), "{json}");
    }
}

#[test]
fn design_status_example_gains_null_session() {
    let json = r#"{"event": "status", "model": "parakeet-tdt-0.6b-v2", "busy": false, "version": "0.1.0",
        "capabilities": {"hotwords": false, "partials": false}}"#;
    let mut expected = value(json);
    expected["session"] = Value::Null;
    assert_eq!(event_roundtrip(json), expected);
}

#[test]
fn session_scoped_events_roundtrip_with_id() {
    for json in [
        r#"{"event": "status", "model": "m", "busy": true, "version": "0.1.0",
            "capabilities": {"hotwords": true, "partials": false}, "session": "s-2", "id": 9}"#,
        r#"{"event": "error", "code": "busy", "message": "m", "session": "s-2", "id": "x"}"#,
        r#"{"event": "ack", "op": "cancel", "session": "s-2"}"#,
    ] {
        assert_eq!(event_roundtrip(json), value(json), "{json}");
    }
}

#[test]
fn ack_serializes_exactly() {
    let out = Out {
        id: Some(json!(7)),
        event: Event::Ack {
            op: AckOp::Stop,
            session: session_id(1),
        },
    };
    let line = to_line(&out);
    assert_eq!(line.matches('\n').count(), 1);
    assert!(line.ends_with('\n'));
    assert_eq!(
        value(&line),
        json!({"event":"ack","op":"stop","session":"s-1","id":7})
    );
}

#[test]
fn enum_wire_names() {
    let reasons = [
        Reason::Stop,
        Reason::Silence,
        Reason::MaxDuration,
        Reason::EndOfFile,
    ];
    assert_eq!(
        serde_json::to_value(reasons).unwrap(),
        json!(["stop", "silence", "max_duration", "end_of_file"])
    );
    let codes = [
        ErrorCode::Busy,
        ErrorCode::NoSession,
        ErrorCode::BadRequest,
        ErrorCode::BadAudio,
        ErrorCode::AudioUnavailable,
        ErrorCode::UnknownPostprocessor,
    ];
    assert_eq!(
        serde_json::to_value(codes).unwrap(),
        json!([
            "busy",
            "no_session",
            "bad_request",
            "bad_audio",
            "audio_unavailable",
            "unknown_postprocessor"
        ])
    );
}

#[test]
fn session_id_roundtrips() {
    for gen in [1, 17, u64::MAX] {
        assert_eq!(parse_session_id(&session_id(gen)), Some(gen));
    }
    assert_eq!(session_id(17), "s-17");
}

#[test]
fn parse_session_id_rejects_garbage() {
    for id in [
        "",
        "s-",
        "s-0",
        "s-abc",
        "s-+5",
        "s-007",
        "s--1",
        "x-1",
        "s-1 ",
        "S-1",
        "s-18446744073709551616",
    ] {
        assert_eq!(parse_session_id(id), None, "{id:?}");
    }
}
