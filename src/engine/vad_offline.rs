//! Silero VAD splits audio into speech segments; each segment is decoded by the
//! offline Parakeet TDT recognizer (DESIGN.md "Streaming strategy").

use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, SileroVadModelConfig, SpeechSegment,
    VadModelConfig, VoiceActivityDetector,
};

use super::{slice_pcm, Engine, EngineEvent, SAMPLE_RATE, WINDOW};
use crate::config::Config;
use crate::protocol::Capabilities;

const HOTWORDS_WARNING: &str = "warning: hotwords enabled; upstream TDT+modified_beam_search \
    bug (k2-fsa/sherpa-onnx#3657) may yield empty output";

pub struct VadOfflineEngine {
    recognizer: OfflineRecognizer,
    vad_cfg: VadModelConfig,
    /// Sized so the VAD buffer always exceeds the longest segment.
    vad_buffer_s: f32,
    vad: VoiceActivityDetector,
    hotwords_enabled: bool,
    config_words: Vec<String>,
    /// Session hotwords: config ∪ request, deduplicated.
    words: Vec<String>,
    max_speech_samples: usize,
    /// Soft bound: flush once `speech_run` reaches this, so that the flushed
    /// segment (run + VAD pre-roll) still fits in `max_speech_samples`.
    flush_at: usize,
    /// Real samples fed this session (padding excluded).
    total_fed: usize,
    /// Consecutive speech samples since the last non-speech window.
    speech_run: usize,
    last_window_speech: bool,
}

fn path_str(p: &std::path::Path) -> Option<String> {
    Some(p.to_string_lossy().into_owned())
}

fn secs(samples: usize) -> f32 {
    samples as f32 / SAMPLE_RATE as f32
}

impl VadOfflineEngine {
    pub fn new(cfg: &Config) -> Result<Self, String> {
        let (m, v, h) = (&cfg.model, &cfg.vad, &cfg.hotwords);
        let file = |stem: &str| {
            m.file(stem)
                .ok_or_else(|| format!("engine: no {stem} model in {}", m.dir.display()))
        };
        let tokens = m.dir.join("tokens.txt");
        if !tokens.is_file() {
            return Err(format!("engine: no tokens.txt in {}", m.dir.display()));
        }

        let mut rc = OfflineRecognizerConfig::default();
        let mc = &mut rc.model_config;
        mc.transducer.encoder = path_str(&file("encoder")?);
        mc.transducer.decoder = path_str(&file("decoder")?);
        mc.transducer.joiner = path_str(&file("joiner")?);
        mc.tokens = path_str(&tokens);
        mc.model_type = Some("nemo_transducer".into());
        mc.num_threads = m.threads;
        mc.provider = Some(m.provider.as_str().into());
        rc.decoding_method = Some("greedy_search".into());
        if h.enabled {
            eprintln!("{HOTWORDS_WARNING}");
            rc.decoding_method = Some("modified_beam_search".into());
            rc.max_active_paths = 4;
            rc.hotwords_score = h.score;
            rc.model_config.modeling_unit = Some("bpe".into());
            rc.model_config.bpe_vocab = path_str(&h.bpe_vocab);
        }
        let recognizer = OfflineRecognizer::create(&rc)
            .ok_or_else(|| format!("engine: failed to load model from {}", m.dir.display()))?;

        let ms = |x: u64| x as f32 / 1000.0;
        let vad_cfg = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: path_str(&v.model),
                threshold: v.threshold,
                min_silence_duration: ms(v.min_silence_ms),
                min_speech_duration: ms(v.min_speech_ms),
                window_size: WINDOW as i32,
                max_speech_duration: ms(v.max_speech_ms),
            },
            sample_rate: SAMPLE_RATE as i32,
            num_threads: 1,
            provider: Some("cpu".into()),
            ..VadModelConfig::default()
        };
        let vad_buffer_s = ms(v.max_speech_ms) + 10.0;
        let vad = create_vad(&vad_cfg, vad_buffer_s)?;
        let samples = |ms: u64| usize::try_from(ms).unwrap_or(usize::MAX).saturating_mul(16);
        let max_speech_samples = samples(v.max_speech_ms);
        // sherpa starts a segment up to 2 windows + min_speech before its first speech window.
        let preroll = 2 * WINDOW + samples(v.min_speech_ms);
        let flush_at = max_speech_samples
            .saturating_sub(preroll + WINDOW)
            .max(WINDOW);
        Ok(Self {
            recognizer,
            vad_cfg,
            vad_buffer_s,
            vad,
            hotwords_enabled: h.enabled,
            config_words: h.words.clone(),
            words: Vec::new(),
            max_speech_samples,
            flush_at,
            total_fed: 0,
            speech_run: 0,
            last_window_speech: false,
        })
    }

    /// Feeds one 512-sample window of which `real` samples are real audio.
    fn feed(&mut self, window: &[f32], real: usize, out: &mut Vec<EngineEvent>) {
        self.vad.accept_waveform(window);
        self.total_fed += real;
        self.last_window_speech = self.vad.detected();
        self.speech_run = if self.last_window_speech {
            self.speech_run + WINDOW
        } else {
            0
        };
        self.drain(out);
        if self.speech_run >= self.flush_at {
            // No `reset()`: flush already empties the buffer, and a reset Silero
            // state needs ~0.8 s to re-detect ongoing speech, which was lost.
            // Without it the next speech window starts a segment exactly here.
            self.vad.flush();
            self.drain(out);
            self.speech_run = 0;
        }
    }

    fn drain(&mut self, out: &mut Vec<EngineEvent>) {
        while let Some(seg) = self.vad.front() {
            self.decode(&seg, out);
            self.vad.pop();
        }
    }

    /// Hard bound: decodes `seg` in pieces of at most `max_speech_samples`.
    fn decode(&self, seg: &SpeechSegment, out: &mut Vec<EngineEvent>) {
        let pcm = seg.samples();
        // The VAD is never reset within a session, so indices are session-absolute.
        let base = usize::try_from(seg.start()).unwrap_or(0);
        let total = secs(self.total_fed);
        for (a, b) in slice_pcm(pcm.len(), self.max_speech_samples) {
            let stream = if self.hotwords_enabled && !self.words.is_empty() {
                self.recognizer
                    .create_stream_with_hotwords(&self.words.join("/"))
            } else {
                self.recognizer.create_stream()
            };
            stream.accept_waveform(SAMPLE_RATE as i32, &pcm[a..b]);
            self.recognizer.decode(&stream);
            let text = stream
                .get_result()
                .map(|r| r.text.trim().to_owned())
                .unwrap_or_default();
            if !text.is_empty() {
                // Same index→seconds mapping for both ends keeps pieces exactly adjacent.
                let (start, end) = (secs(base + a).min(total), secs(base + b).min(total));
                out.push(EngineEvent::Segment { text, start, end });
            }
        }
    }
}

fn create_vad(cfg: &VadModelConfig, buffer_s: f32) -> Result<VoiceActivityDetector, String> {
    VoiceActivityDetector::create(cfg, buffer_s).ok_or_else(|| {
        let model = cfg.silero_vad.model.as_deref().unwrap_or_default();
        format!("engine: failed to load VAD model {model}")
    })
}

impl Engine for VadOfflineEngine {
    fn begin(&mut self, hotwords: &[String]) {
        // The model loaded in `new`, so recreation with the same config only fails on OOM.
        if let Ok(vad) = create_vad(&self.vad_cfg, self.vad_buffer_s) {
            self.vad = vad;
        } else {
            self.vad.reset();
        }
        (self.total_fed, self.speech_run) = (0, 0);
        self.last_window_speech = false;
        let mut words: Vec<String> = Vec::new();
        for w in self.config_words.iter().chain(hotwords) {
            if !words.contains(w) {
                words.push(w.clone());
            }
        }
        self.words = words;
    }

    fn accept(&mut self, samples: &[f32]) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        for chunk in samples.chunks(WINDOW) {
            if chunk.len() == WINDOW {
                self.feed(chunk, WINDOW, &mut out);
            } else {
                let mut padded = [0.0; WINDOW];
                padded[..chunk.len()].copy_from_slice(chunk);
                self.feed(&padded, chunk.len(), &mut out);
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        self.vad.flush();
        self.drain(&mut out);
        out
    }

    fn in_speech(&self) -> bool {
        self.last_window_speech
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hotwords: self.hotwords_enabled,
            partials: false,
        }
    }
}
