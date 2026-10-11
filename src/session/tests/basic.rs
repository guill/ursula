//! Request handling: lifecycle, ownership, ids, validation, connection teardown.

use super::*;

#[test]
fn t01_push_happy_path() {
    let mut h = h(FakeMode::EverySecond);
    let outs = h.req(1, PUSH);
    assert_eq!(one(&outs, 1, "started")["session"], "s-1");
    for _ in 0..3 {
        h.push(1, &speech(16000));
    }
    let outs = h.req(1, STOP);
    let events = sent(&outs, 1);
    assert_eq!(events[0]["event"], "ack");
    assert_eq!(events[1]["event"], "done");
    let done = &events[1];
    assert_eq!(done["reason"], "stop");
    assert_eq!(done["text"], "seg1 seg2 seg3");
    assert_eq!(done["raw_text"], "seg1 seg2 seg3");
    assert_eq!(done["segments"].as_array().unwrap().len(), 3);
    assert_eq!(done["audio_ms"], 3000);
    assert!(!h.busy());
}

#[test]
fn t02_second_start_busy() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, PUSH);
    let outs = h.req(2, PUSH);
    let err = one(&outs, 2, "error");
    assert_eq!(err["code"], "busy");
    assert_eq!(err["session"], "s-1");
    assert_eq!(tags(&outs), ["send:2:error"]);
}

#[test]
fn t03_stop_from_non_owner() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, PUSH);
    let outs = h.req(2, STOP);
    assert_eq!(one(&outs, 2, "ack")["op"], "stop");
    assert_eq!(sent(&outs, 2).len(), 1);
    assert_eq!(one(&outs, 1, "done")["reason"], "stop");
}

#[test]
fn t04_wrong_session_no_session() {
    let mut h = h(FakeMode::EverySecond);
    let outs = h.req(1, STOP);
    assert_eq!(one(&outs, 1, "error")["code"], "no_session");
    h.req(1, PUSH);
    for op in ["stop", "cancel"] {
        let outs = h.req(2, &format!(r#"{{"op":"{op}","session":"s-9"}}"#));
        assert_eq!(one(&outs, 2, "error")["code"], "no_session");
        assert_eq!(tags(&outs).len(), 1);
    }
    assert!(h.busy());
    let outs = h.req(2, r#"{"op":"stop","session":"s-1"}"#);
    one(&outs, 1, "done");
}

#[test]
fn t05_cancel() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, PUSH);
    h.push(1, &speech(20000));
    let outs = h.req(1, CANCEL);
    assert_eq!(tags(&outs)[..2], ["send:1:ack", "send:1:cancelled"]);
    assert!(sent(&outs, 1).iter().all(|v| v["event"] != "done"));
    assert!(!h.busy());
    assert_eq!(
        h.ctl
            .cancel_hint()
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn t06_owner_conn_closed_cancels_silently() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, PUSH);
    let outs = h.input(Input::ConnClosed(1));
    assert!(sent(&outs, 1).is_empty());
    assert!(!h.busy());
    let outs = h.req(2, PUSH);
    assert_eq!(one(&outs, 2, "started")["session"], "s-2");
}

#[test]
fn t14_unknown_postprocessor() {
    let mut h = h(FakeMode::EverySecond);
    let outs = h.req(
        1,
        r#"{"op":"start","source":"push","postprocess":["nope"]}"#,
    );
    assert_eq!(one(&outs, 1, "error")["code"], "unknown_postprocessor");
    assert!(!h.busy());
    assert_eq!(h.log().begins, 0);
    let outs = h.req(1, r#"{"op":"start","source":"push","max_ms":0}"#);
    assert_eq!(one(&outs, 1, "error")["code"], "bad_request");
    assert!(!h.busy());
}

#[test]
fn t15_hotwords_disabled_warning() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, r#"{"op":"start","source":"push","hotwords":["NixOS"]}"#);
    assert_eq!(h.log().hotwords, ["NixOS"]);
    let done = one(&h.req(1, STOP), 1, "done");
    assert_eq!(
        done["warnings"][0],
        "hotwords ignored: not enabled for this model"
    );
}

#[test]
fn t26_nul_and_empty_hotwords_dropped() {
    let mut h = h(FakeMode::EverySecond);
    let words = r#"["ok","bad\u0000word",""]"#;
    h.req(
        1,
        &format!(r#"{{"op":"start","source":"push","postprocess":["p"],"hotwords":{words}}}"#),
    );
    assert_eq!(h.log().hotwords, ["ok"]);
    let outs = h.req(1, STOP);
    let Some(Output::RunPost { job, .. }) =
        outs.iter().find(|o| matches!(o, Output::RunPost { .. }))
    else {
        panic!("no RunPost: {:?}", tags(&outs));
    };
    assert_eq!(job.hotwords, ["ok"]);
}

#[test]
fn t16_id_echo() {
    let mut h = h(FakeMode::EverySecond);
    let outs = h.req(1, r#"{"op":"start","source":"push","id":"a"}"#);
    assert_eq!(one(&outs, 1, "started")["id"], "a");
    let outs = h.req(2, r#"{"op":"start","source":"push","id":9}"#);
    assert_eq!(one(&outs, 2, "error")["id"], 9);
    let outs = h.req(2, r#"{"op":"stop","id":5}"#);
    assert_eq!(one(&outs, 2, "ack")["id"], 5);
    assert_eq!(one(&outs, 1, "done")["id"], "a");
    let outs = h.req(2, r#"{"op":"status","id":3}"#);
    let status = one(&outs, 2, "status");
    assert_eq!(
        (status["id"].clone(), status["busy"].clone()),
        (3.into(), false.into())
    );
}

#[test]
fn t17_half_close() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, PUSH);
    assert!(h.input(Input::ReadClosed(1)).is_empty());
    assert!(h.busy());
    assert_eq!(tags(&h.input(Input::ReadClosed(3))), ["close:3"]);
    let tags = tags(&h.req(2, STOP));
    let done = tags.iter().position(|t| t == "send:1:done").unwrap();
    let close = tags.iter().position(|t| t == "close:1").unwrap();
    assert!(done < close, "{tags:?}");
}

#[test]
fn t24_file_source() {
    let dir = std::env::temp_dir().join(format!("ursula-t24-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    w.write_sample(0i16).unwrap();
    w.finalize().unwrap();
    let mut h = h(FakeMode::EverySecond);
    let bad = format!(
        r#"{{"op":"start","source":{{"file":"{}"}}}}"#,
        dir.join("x.wav").display()
    );
    assert_eq!(one(&h.req(1, &bad), 1, "error")["code"], "bad_audio");
    let line = format!(
        r#"{{"op":"start","source":{{"file":"{}"}}}}"#,
        path.display()
    );
    let outs = h.req(1, &line);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(tags(&outs), ["send:1:started", "start_file:1", "publish"]);
    h.msg(AudioMsg::Samples {
        gen: 1,
        data: vec![0.5; 16000],
    });
    let outs = h.msg(AudioMsg::Eof { gen: 1 });
    assert_eq!(one(&outs, 1, "done")["reason"], "end_of_file");
    assert!(tags(&outs).contains(&"stop_capture:1".to_owned()));
}
