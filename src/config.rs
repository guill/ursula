//! TOML configuration (DESIGN.md "Configuration").
//!
//! [`load`] parses, fills derived defaults and validates. Afterwards every field
//! is concrete: `model.name`, `model.threads` and `hotwords.bpe_vocab` hold
//! their resolved values (an empty `name`/`bpe_vocab` in the file means "derive").

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: Model,
    pub vad: Vad,
    pub session: Session,
    pub hotwords: Hotwords,
    pub postprocessors: BTreeMap<String, Postprocessor>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Model {
    pub kind: ModelKind,
    pub dir: PathBuf,
    /// Default: `dir` basename without the `sherpa-onnx-nemo-` prefix.
    pub name: String,
    pub threads: i32,
    pub provider: Provider,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum ModelKind {
    #[default]
    #[serde(rename = "parakeet-tdt")]
    ParakeetTdt,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Cpu,
    Cuda,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Vad {
    pub model: PathBuf,
    pub threshold: f32,
    pub min_silence_ms: u64,
    pub min_speech_ms: u64,
    pub max_speech_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Session {
    pub max_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hotwords {
    pub enabled: bool,
    pub score: f32,
    pub words: Vec<String>,
    /// Default: `<model.dir>/bpe.vocab`.
    pub bpe_vocab: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Postprocessor {
    pub command: Vec<String>,
    pub timeout_ms: u64,
}

impl Default for Model {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Self {
            kind: ModelKind::default(),
            dir: PathBuf::new(),
            name: String::new(),
            threads: cpus.min(4) as i32, // <= 4: lossless
            provider: Provider::default(),
        }
    }
}

impl Default for Vad {
    fn default() -> Self {
        Self {
            model: PathBuf::new(),
            threshold: 0.5,
            min_silence_ms: 500,
            min_speech_ms: 250,
            max_speech_ms: 20_000,
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self { max_ms: 300_000 }
    }
}

impl Default for Hotwords {
    fn default() -> Self {
        Self {
            enabled: false,
            score: 1.5,
            words: Vec::new(),
            bpe_vocab: PathBuf::new(),
        }
    }
}

impl Default for Postprocessor {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            timeout_ms: 10_000,
        }
    }
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
        }
    }
}

impl Model {
    /// `<dir>/<stem>.int8.onnx` if it exists, else `<dir>/<stem>.onnx` if it exists.
    pub fn file(&self, stem: &str) -> Option<PathBuf> {
        [".int8.onnx", ".onnx"]
            .iter()
            .map(|ext| self.dir.join(format!("{stem}{ext}")))
            .find(|p| p.is_file())
    }
}

/// `$XDG_CONFIG_HOME/ursula/config.toml`, else `$HOME/.config/ursula/config.toml`.
pub fn default_path() -> PathBuf {
    let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config");
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(home)
        .join("ursula/config.toml")
}

pub fn load(path: &Path) -> Result<Config, String> {
    let at = |e: &dyn std::fmt::Display| format!("config: {}: {e}", path.display());
    let text = fs::read_to_string(path).map_err(|e| at(&e))?;
    let mut cfg: Config = toml::from_str(&text).map_err(|e| at(&e))?;
    if cfg.model.name.is_empty() {
        let base = cfg.model.dir.file_name().unwrap_or_default();
        let base = base.to_string_lossy();
        let name = base.strip_prefix("sherpa-onnx-nemo-").unwrap_or(&base);
        cfg.model.name = name.to_owned();
    }
    if cfg.hotwords.bpe_vocab.as_os_str().is_empty() {
        cfg.hotwords.bpe_vocab = cfg.model.dir.join("bpe.vocab");
    }
    cfg.validate()?;
    Ok(cfg)
}

fn ensure(ok: bool, field: &str, reason: &str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(format!("config: {field}: {reason}"))
    }
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        let (m, v, h) = (&self.model, &self.vad, &self.hotwords);
        let dir = m.dir.display();
        ensure(!m.dir.as_os_str().is_empty(), "model.dir", "is required")?;
        for stem in ["encoder", "decoder", "joiner"] {
            let why = format!("no {stem}.int8.onnx or {stem}.onnx in {dir}");
            ensure(m.file(stem).is_some(), "model.dir", &why)?;
        }
        let tokens = m.dir.join("tokens.txt").is_file();
        ensure(tokens, "model.dir", &format!("no tokens.txt in {dir}"))?;
        ensure(m.threads > 0, "model.threads", "must be > 0")?;

        ensure(!v.model.as_os_str().is_empty(), "vad.model", "is required")?;
        let why = format!("no such file: {}", v.model.display());
        ensure(v.model.is_file(), "vad.model", &why)?;
        let threshold_ok = v.threshold > 0.0 && v.threshold < 1.0;
        ensure(threshold_ok, "vad.threshold", "must be in (0, 1)")?;
        let speech_ok = v.max_speech_ms > v.min_speech_ms;
        let why = "must be > vad.min_speech_ms";
        ensure(speech_ok, "vad.max_speech_ms", why)?;
        ensure(self.session.max_ms > 0, "session.max_ms", "must be > 0")?;

        let vocab_ok = !h.enabled || h.bpe_vocab.is_file();
        let why = format!(
            "no such file (needed when hotwords.enabled): {}",
            h.bpe_vocab.display()
        );
        ensure(vocab_ok, "hotwords.bpe_vocab", &why)?;
        for (name, p) in &self.postprocessors {
            let field = format!("postprocessors.{name}.command");
            ensure(!p.command.is_empty(), &field, "must be non-empty")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
