//! Mic lifecycle, supervision ticks, post chain and late inputs.

use super::*;

const MIC_POST: &str = r#"{"op":"start","source":"mic","postprocess":["p"]}"#;

#[test]
fn t09_mic_open_timeout_and_stop_while_opening() {
    let mut h = h(FakeMode::EverySecond);
    let outs = h.req(1, MIC);
    assert_eq!(tags(&outs), ["open_mic:1", "publish"]);
    assert!(h.busy());
    let outs = h.msg(AudioMsg::Opened { gen: 1 });
    assert_eq!(one(&outs, 1, "started")["session"], "s-1");

    let mut h = super::h(FakeMode::EverySecond);
    h.req(1, MIC);
    assert!(h.tick(2000, 0).is_empty());
    let err = one(&h.tick(2001, 0), 1, "error");
    assert_eq!(
        (err["code"].clone(), err["session"].clone()),
        ("audio_unavailable".into(), "s-1".into())
    );
    assert!(!h.busy());

    let mut h = super::h(FakeMode::EverySecond);
    h.req(1, MIC);
    let outs = h.req(2, STOP);
    assert_eq!(tags(&outs)[..2], ["send:2:ack", "send:1:cancelled"]);
    assert!(h.msg(AudioMsg::Opened { gen: 1 }).is_empty());
}

#[test]
fn t10_mic_data_stall_only_while_recording() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, MIC);
    h.now = 100;
    h.msg(AudioMsg::Opened { gen: 1 });
    assert!(h.tick(2100, 0).is_empty());
    let err = one(&h.tick(2101, 0), 1, "error");
    assert_eq!(err["message"], "no audio for 2 s");

    let mut h = super::h(FakeMode::EverySecond);
    h.req(1, MIC_POST);
    h.msg(AudioMsg::Opened { gen: 1 });
    h.msg(AudioMsg::Samples {
        gen: 1,
        data: vec![0.5; 16000],
    });
    let outs = h.req(1, STOP);
    assert!(tags(&outs).contains(&"run_post:1".to_owned()));
    assert!(h.tick(10_000, 0).is_empty());
    let post = Input::PostDone {
        gen: 1,
        text: "X".into(),
        warnings: vec![],
    };
    assert_eq!(one(&h.input(post), 1, "done")["text"], "X");
}

#[test]
fn t11_mic_worker_bound() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, MIC);
    let outs = h.req(1, CANCEL);
    assert!(tags(&outs).contains(&"stop_capture:1".to_owned()));
    let err = one(&h.req(1, MIC), 1, "error");
    assert_eq!(err["code"], "audio_unavailable");
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("has not shut down"));
    one(&h.req(2, PUSH), 2, "started");
    one(&h.req(2, CANCEL), 2, "cancelled");
    assert!(h.msg(AudioMsg::MicExited { gen: 1 }).is_empty());
    assert_eq!(tags(&h.req(1, MIC)), ["open_mic:3", "publish"]);
}

#[test]
fn t12_post_chain() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, r#"{"op":"start","source":"push","postprocess":["p"]}"#);
    h.push(1, &speech(16000));
    let outs = h.req(1, STOP);
    let Some(Output::RunPost { gen: 1, job }) =
        outs.iter().find(|o| matches!(o, Output::RunPost { .. }))
    else {
        panic!("no RunPost: {:?}", tags(&outs));
    };
    assert_eq!((job.text.as_str(), job.chain[0].0.as_str()), ("seg1", "p"));
    assert!(sent(&outs, 1).iter().all(|v| v["event"] != "done"));
    assert!(h.busy());
    let post = Input::PostDone {
        gen: 1,
        text: "SEG ONE".into(),
        warnings: vec!["w".into()],
    };
    let done = one(&h.input(post), 1, "done");
    assert_eq!(
        (done["text"].clone(), done["raw_text"].clone()),
        ("SEG ONE".into(), "seg1".into())
    );
    assert_eq!(done["warnings"], serde_json::json!(["w"]));

    h.req(1, r#"{"op":"start","source":"push","postprocess":["p"]}"#);
    h.req(1, STOP);
    assert_eq!(tags(&h.req(2, STOP)), ["send:2:ack"]);
    let outs = h.req(2, CANCEL);
    assert_eq!(
        tags(&outs)[..3],
        ["send:2:ack", "cancel_post:2", "send:1:cancelled"]
    );
    let late = Input::PostDone {
        gen: 2,
        text: "late".into(),
        warnings: vec![],
    };
    assert!(h.input(late).is_empty());
}

#[test]
fn t13_late_inputs_ignored() {
    let mut h = h(FakeMode::EverySecond);
    h.req(1, r#"{"op":"start","source":"mic","postprocess":["p"]}"#);
    h.msg(AudioMsg::Opened { gen: 1 });
    h.req(1, STOP);
    let before = (h.ctl.snapshot(), h.log());
    assert!(h.msg(AudioMsg::Eof { gen: 1 }).is_empty());
    assert!(h
        .msg(AudioMsg::Samples {
            gen: 1,
            data: vec![0.5; 2048]
        })
        .is_empty());
    assert!(h
        .msg(AudioMsg::Failed {
            gen: 1,
            msg: "bad_audio: x".into()
        })
        .is_empty());
    assert_eq!((h.ctl.snapshot(), h.log()), before);

    h.req(1, CANCEL);
    h.req(2, PUSH);
    let accepts = h.log().accepts;
    assert!(h
        .msg(AudioMsg::Samples {
            gen: 1,
            data: vec![0.5; 2048]
        })
        .is_empty());
    assert!(h.msg(AudioMsg::Eof { gen: 1 }).is_empty());
    assert_eq!(h.log().accepts, accepts);
    assert!(h.busy());
}
