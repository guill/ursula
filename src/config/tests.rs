use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const MODEL: &str = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8";

/// Unique temp dir with fake model files; removed on drop.
struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("ursula-config-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(MODEL)).unwrap();
        for f in [
            "encoder.int8.onnx",
            "decoder.int8.onnx",
            "joiner.int8.onnx",
            "tokens.txt",
        ] {
            fs::write(root.join(MODEL).join(f), "").unwrap();
        }
        fs::write(root.join("silero_vad.onnx"), "").unwrap();
        Self(root)
    }

    /// Loads the minimal config plus `extra` (root-level dotted keys).
    fn load(&self, extra: &str) -> Result<Config, String> {
        let (dir, vad) = (self.0.join(MODEL), self.0.join("silero_vad.onnx"));
        let path = self.0.join("config.toml");
        let text = format!("model.dir = {dir:?}\nvad.model = {vad:?}\n{extra}\n");
        fs::write(&path, text).unwrap();
        load(&path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_field_err(res: Result<Config, String>, field: &str) {
    let err = res.unwrap_err();
    assert!(err.starts_with(&format!("config: {field}: ")), "{err}");
}

#[test]
fn minimal_config_gets_defaults() {
    let fx = Fixture::new();
    let cfg = fx.load("").unwrap();
    let cpus = std::thread::available_parallelism().unwrap().get().min(4);
    assert_eq!(cfg.model.kind, ModelKind::ParakeetTdt);
    assert_eq!(usize::try_from(cfg.model.threads).unwrap(), cpus);
    assert_eq!(cfg.model.provider, Provider::Cpu);
    let v = &cfg.vad;
    let vad = (
        v.threshold,
        v.min_silence_ms,
        v.min_speech_ms,
        v.max_speech_ms,
    );
    assert_eq!(vad, (0.5, 500, 250, 20_000));
    assert_eq!(cfg.session.max_ms, 300_000);
    let h = &cfg.hotwords;
    assert_eq!((h.enabled, h.score, h.words.len()), (false, 1.5, 0));
    assert_eq!(h.bpe_vocab, fx.0.join(MODEL).join("bpe.vocab"));
    assert!(cfg.postprocessors.is_empty());
}

#[test]
fn name_defaults_to_dir_basename_without_prefix() {
    let cfg = Fixture::new().load("").unwrap();
    assert_eq!(cfg.model.name, "parakeet-tdt-0.6b-v2-int8");
}

#[test]
fn explicit_name_is_kept() {
    let cfg = Fixture::new().load("model.name = \"custom\"").unwrap();
    assert_eq!(cfg.model.name, "custom");
}

#[test]
fn unknown_key_is_rejected() {
    let err = Fixture::new().load("model.bogus = 1").unwrap_err();
    assert!(err.contains("unknown field `bogus`"), "{err}");
}

#[test]
fn unknown_kind_is_rejected() {
    let err = Fixture::new().load("model.kind = \"whisper\"").unwrap_err();
    assert!(err.contains("unknown variant `whisper`"), "{err}");
}

#[test]
fn threshold_out_of_range_is_rejected() {
    assert_field_err(Fixture::new().load("vad.threshold = 2"), "vad.threshold");
}

#[test]
fn missing_model_file_is_rejected() {
    let fx = Fixture::new();
    fs::remove_file(fx.0.join(MODEL).join("joiner.int8.onnx")).unwrap();
    assert_field_err(fx.load(""), "model.dir");
}

#[test]
fn hotwords_enabled_without_bpe_vocab_is_rejected() {
    let res = Fixture::new().load("hotwords.enabled = true");
    assert_field_err(res, "hotwords.bpe_vocab");
}

#[test]
fn hotwords_enabled_with_bpe_vocab_loads() {
    let fx = Fixture::new();
    fs::write(fx.0.join(MODEL).join("bpe.vocab"), "").unwrap();
    assert!(fx.load("hotwords.enabled = true").unwrap().hotwords.enabled);
}

#[test]
fn empty_postprocessor_command_is_rejected() {
    let res = Fixture::new().load("postprocessors.cleanup.command = []");
    assert_field_err(res, "postprocessors.cleanup.command");
}

#[test]
fn postprocessor_timeout_defaults_to_10s() {
    let res = Fixture::new().load("postprocessors.cleanup.command = [\"true\"]");
    assert_eq!(res.unwrap().postprocessors["cleanup"].timeout_ms, 10_000);
}
