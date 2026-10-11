use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{env, fs, process};

use serde_json::{json, Value};

use super::*;

const STARTED: &str = r#"{"event":"started","session":"s-1","model":"parakeet"}"#;

fn mic(stream: bool) -> Request {
    Request::Start {
        source: Source::Mic,
        stream,
        hotwords: Vec::new(),
        context: None,
        postprocess: Vec::new(),
        max_ms: None,
        silence_stop_ms: None,
    }
}

fn socket(name: &str) -> PathBuf {
    env::temp_dir().join(format!("ursula-client-{}-{name}.sock", process::id()))
}

fn listen(name: &str) -> (PathBuf, UnixListener) {
    let path = socket(name);
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    (path, listener)
}

/// One-connection fake daemon: for each script step it reads one request
/// line, then writes that step's raw event lines. Returning closes the
/// connection (the client's EOF). Yields the requests it read.
fn fake_daemon(
    name: &str,
    script: &'static [&'static [&'static str]],
) -> (PathBuf, JoinHandle<Vec<Value>>) {
    let (path, listener) = listen(name);
    let daemon = thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = BufReader::new(&conn);
        let step = |replies: &&[&str]| {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            for reply in *replies {
                (&conn).write_all(format!("{reply}\n").as_bytes()).unwrap();
            }
            serde_json::from_str(&line).unwrap()
        };
        script.iter().map(step).collect()
    });
    (path, daemon)
}

#[test]
fn outcome_prints_segment_in_stream_mode() {
    let event = json!({"event": "segment", "session": "s-1", "text": "hi"});
    assert_eq!(outcome(&event, true), Some(Outcome::Segment("hi".into())));
}

#[test]
fn outcome_ignores_segment_without_stream() {
    let event = json!({"event": "segment", "session": "s-1", "text": "hi"});
    assert_eq!(outcome(&event, false), None);
}

#[test]
fn outcome_prints_done_text_without_stream() {
    let event = json!({"event": "done", "session": "s-1", "text": "Final."});
    assert_eq!(
        outcome(&event, false),
        Some(Outcome::Done(Some("Final.".into())))
    );
}

#[test]
fn outcome_does_not_print_done_text_in_stream_mode() {
    let event = json!({"event": "done", "session": "s-1", "text": "Final."});
    assert_eq!(outcome(&event, true), Some(Outcome::Done(None)));
}

#[test]
fn outcome_fails_on_cancelled() {
    let event = json!({"event": "cancelled", "session": "s-1"});
    assert_eq!(
        outcome(&event, false),
        Some(Outcome::Fail("cancelled".into()))
    );
}

#[test]
fn outcome_fails_on_error_with_code_and_message() {
    let event = json!({"event": "error", "code": "audio_unavailable", "message": "no mic", "session": "s-1"});
    assert_eq!(
        outcome(&event, true),
        Some(Outcome::Fail("error: audio_unavailable: no mic".into()))
    );
}

#[test]
fn outcome_ignores_started_and_ack() {
    let started = json!({"event": "started", "session": "s-1", "model": "m"});
    let ack = json!({"event": "ack", "op": "stop", "session": "s-1"});
    assert_eq!(
        [outcome(&started, false), outcome(&ack, false)],
        [None, None]
    );
}

#[test]
fn stop_succeeds_on_ack() {
    let (path, daemon) = fake_daemon(
        "stop-ack",
        &[&[r#"{"event":"ack","op":"stop","session":"s-1"}"#]],
    );
    assert_eq!(control(&path, Request::Stop { session: None }), Ok(()));
    assert_eq!(daemon.join().unwrap(), [json!({"op": "stop"})]);
}

#[test]
fn cancel_fails_on_error_reply() {
    let script: &[&[&str]] =
        &[&[r#"{"event":"error","code":"no_session","message":"no active session"}"#]];
    let (path, _daemon) = fake_daemon("cancel-error", script);
    let result = control(&path, Request::Cancel { session: None });
    assert_eq!(result, Err("error: no_session: no active session".into()));
}

#[test]
fn toggle_stops_the_busy_session_by_id_and_prints_nothing() {
    const BUSY: &str =
        r#"{"event":"error","code":"busy","message":"a session is active","session":"s-7"}"#;
    const ACK: &str = r#"{"event":"ack","op":"stop","session":"s-7"}"#;
    let (path, daemon) = fake_daemon("toggle-busy", &[&[BUSY], &[ACK]]);
    let mut out = Vec::new();
    assert_eq!(toggle(&path, mic(false), &mut out), Ok(()));
    assert!(out.is_empty());
    let requests = daemon.join().unwrap();
    assert_eq!(
        (&requests[0]["op"], &requests[0]["source"]),
        (&json!("start"), &json!("mic"))
    );
    assert_eq!(requests[1], json!({"op": "stop", "session": "s-7"}));
}

#[test]
fn toggle_reports_a_first_reply_error_other_than_busy() {
    const ERROR: &str =
        r#"{"event":"error","code":"unknown_postprocessor","message":"no such post-processor: x"}"#;
    let (path, _daemon) = fake_daemon("toggle-error", &[&[ERROR]]);
    let result = toggle(&path, mic(false), &mut Vec::new());
    assert_eq!(
        result,
        Err("error: unknown_postprocessor: no such post-processor: x".into())
    );
}

#[test]
fn record_stream_prints_each_segment_and_not_done_text() {
    let script: &[&[&str]] = &[&[
        STARTED,
        r#"{"event":"segment","session":"s-1","text":"one","start":0.0,"end":1.0}"#,
        r#"{"event":"ack","op":"stop","session":"s-1"}"#,
        r#"{"event":"segment","session":"s-1","text":"two","start":1.0,"end":2.0}"#,
        r#"{"event":"done","session":"s-1","text":"One, two.","raw_text":"one two"}"#,
    ]];
    let (path, _daemon) = fake_daemon("record-stream", script);
    let mut out = Vec::new();
    assert_eq!(record(&path, mic(true), &mut out), Ok(()));
    assert_eq!(String::from_utf8(out).unwrap(), "one\ntwo\n");
}

#[test]
fn record_fails_when_the_daemon_closes_before_a_terminal_event() {
    let (path, _daemon) = fake_daemon("record-eof", &[&[STARTED]]);
    let result = record(&path, mic(false), &mut Vec::new());
    assert_eq!(result, Err(CLOSED.into()));
}

#[test]
fn file_rejects_a_missing_path_without_connecting() {
    let result = file(
        &socket("file-missing"),
        Path::new("/nonexistent/ursula.wav"),
        &mut Vec::new(),
    );
    assert!(result
        .unwrap_err()
        .starts_with("error: cannot open /nonexistent/ursula.wav: "));
}

#[test]
fn status_prints_the_reply_line_verbatim() {
    const LINE: &str =
        r#"{"event":"status","model":"parakeet","busy":false,"session":null,"version":"0.1.0"}"#;
    let (path, daemon) = fake_daemon("status", &[&[LINE]]);
    let mut out = Vec::new();
    assert_eq!(status(&path, &mut out), Ok(()));
    assert_eq!(String::from_utf8(out).unwrap(), format!("{LINE}\n"));
    assert_eq!(daemon.join().unwrap(), [json!({"op": "status"})]);
}

#[test]
fn connect_failure_names_the_socket_and_the_daemon() {
    let path = socket("absent");
    let err = control(&path, Request::Status).unwrap_err();
    assert!(
        err.starts_with(&format!("error: cannot connect to {}: ", path.display())),
        "{err}"
    );
    assert!(err.ends_with(" (is 'ursula serve' running?)"), "{err}");
}

#[test]
fn raw_half_closes_after_input_and_copies_events_until_eof() {
    let (path, listener) = listen("raw");
    let daemon = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut requests = String::new();
        conn.read_to_string(&mut requests).unwrap(); // EOF only after shutdown(Write)
        conn.write_all(b"{\"event\":\"a\"}\n{\"event\":\"b\"}\n")
            .unwrap();
        requests
    });
    let input = "{\"op\":\"status\"}\n{\"op\":\"stop\"}\n";
    let mut out = Vec::new();
    assert_eq!(raw(&path, Cursor::new(input), &mut out), Ok(()));
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "{\"event\":\"a\"}\n{\"event\":\"b\"}\n"
    );
    assert_eq!(daemon.join().unwrap(), input);
}
