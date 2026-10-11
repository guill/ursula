//! Audio input helpers shared by the push, file and mic sources (DESIGN.md "Requests").
//! Only 16 kHz mono s16 PCM is accepted; there is no resampling or channel conversion.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hound::{SampleFormat, WavReader};

/// 100 ms at 16 kHz.
const CHUNK: usize = 1600;

/// Messages from an audio source thread to the controller. Every message carries the session
/// generation so the controller can drop late messages from a finished session.
#[derive(Debug)]
pub enum AudioMsg {
    Opened {
        gen: u64,
    },
    Samples {
        gen: u64,
        data: Vec<f32>,
    },
    Eof {
        gen: u64,
    },
    /// `msg` starts with `bad_audio:` or `audio_unavailable:`, which names the error code.
    Failed {
        gen: u64,
        msg: String,
    },
    /// The mic reader thread has fully exited and dropped its stream.
    MicExited {
        gen: u64,
    },
}

/// Delivers a message; returns false once the receiver is gone, which stops the source.
pub type AudioSink = Box<dyn Fn(AudioMsg) -> bool + Send>;

pub fn s16le_to_f32(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(format!("bad_audio: odd PCM byte count {}", bytes.len()));
    }
    Ok(pairs
        .iter()
        .map(|&b| f32::from(i16::from_le_bytes(b)) / 32768.0)
        .collect())
}

pub fn decode_push(b64: &str) -> Result<Vec<f32>, String> {
    let bytes = STANDARD
        .decode(b64)
        .map_err(|e| format!("bad_audio: invalid base64: {e}"))?;
    s16le_to_f32(&bytes)
}

/// Opens a WAV file and checks its header; samples are read later by the feeder.
pub fn open_wav(path: &Path) -> Result<WavReader<BufReader<File>>, String> {
    // Opening a FIFO blocks the controller until a writer appears. Racy (TOCTOU), but
    // only the socket's own user can swap the path.
    let meta = std::fs::metadata(path).map_err(|e| format!("bad_audio: {e}"))?;
    if !meta.is_file() {
        return Err(format!("bad_audio: not a regular file: {}", path.display()));
    }
    let reader = WavReader::open(path).map_err(|e| format!("bad_audio: {e}"))?;
    let s = reader.spec();
    if (
        s.channels,
        s.sample_rate,
        s.bits_per_sample,
        s.sample_format,
    ) != (1, 16000, 16, SampleFormat::Int)
    {
        return Err(format!(
            "bad_audio: need 16 kHz mono s16 WAV, got {} Hz, {} ch, {}-bit {:?}; \
             convert with: ffmpeg -i IN -ar 16000 -ac 1 -c:a pcm_s16le OUT.wav",
            s.sample_rate, s.channels, s.bits_per_sample, s.sample_format
        ));
    }
    Ok(reader)
}

/// Streams the file as `Samples` chunks then `Eof` (or `Failed` on a read error) from a
/// detached thread. It exits without sending once `stop` is set or the sink is gone.
pub fn spawn_file_feeder(
    reader: WavReader<BufReader<File>>,
    gen: u64,
    send: AudioSink,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut samples = reader.into_samples::<i16>();
        loop {
            let chunk: Result<Vec<f32>, hound::Error> = samples
                .by_ref()
                .take(CHUNK)
                .map(|s| s.map(|s| f32::from(s) / 32768.0))
                .collect();
            let msg = match chunk {
                Err(e) => AudioMsg::Failed {
                    gen,
                    msg: format!("bad_audio: {e}"),
                },
                Ok(data) if data.is_empty() => AudioMsg::Eof { gen },
                Ok(data) => AudioMsg::Samples { gen, data },
            };
            let last = !matches!(msg, AudioMsg::Samples { .. });
            if stop.load(Ordering::Relaxed) || !send(msg) || last {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::mpsc;

    fn wav(name: &str, sample_rate: u32, n: usize) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("ursula-audio-{}-{name}.wav", std::process::id()));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..n {
            w.write_sample(i16::try_from(i % 1000).unwrap()).unwrap();
        }
        w.finalize().unwrap();
        path
    }

    /// Runs the feeder to completion; `stop_after_first` sets the stop flag inside the sink.
    fn feed(path: &Path, stop_after_first: bool) -> Vec<AudioMsg> {
        let reader = open_wav(path).unwrap();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let sink: AudioSink = Box::new(move |m| {
            flag.store(stop_after_first, Ordering::Relaxed);
            tx.send(m).is_ok()
        });
        spawn_file_feeder(reader, 7, sink, stop);
        rx.iter().collect() // ends when the thread exits and drops the sink
    }

    fn sample_count(msgs: &[AudioMsg]) -> usize {
        msgs.iter()
            .map(|m| match m {
                AudioMsg::Samples { gen: 7, data } => data.len(),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn pcm_min_is_minus_one() {
        assert_eq!(s16le_to_f32(&[0x00, 0x80]).unwrap(), vec![-1.0]);
    }

    #[test]
    fn pcm_odd_length_is_err() {
        assert!(s16le_to_f32(&[0, 0, 0])
            .unwrap_err()
            .starts_with("bad_audio:"));
    }

    #[test]
    fn push_invalid_base64_is_err() {
        assert!(decode_push("not base64!")
            .unwrap_err()
            .starts_with("bad_audio:"));
        assert_eq!(decode_push("AIA=").unwrap(), vec![-1.0]);
    }

    #[test]
    fn wav_44k_is_rejected_with_ffmpeg_hint() {
        let path = wav("44k", 44100, 10);
        let err = open_wav(&path).err().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(
            err.starts_with("bad_audio: need 16 kHz mono s16 WAV, got 44100 Hz"),
            "{err}"
        );
        assert!(
            err.contains("ffmpeg -i IN -ar 16000 -ac 1 -c:a pcm_s16le OUT.wav"),
            "{err}"
        );
    }

    #[test]
    fn wav_missing_is_bad_audio() {
        let err = open_wav(Path::new("/nonexistent/ursula.wav"))
            .err()
            .unwrap();
        assert!(err.starts_with("bad_audio: "), "{err}");
    }

    #[test]
    fn wav_fifo_or_dir_is_rejected_promptly() {
        let dir = std::env::temp_dir().join(format!("ursula-audio-{}-fifo", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("f.wav");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(made.unwrap().success());
        for path in [fifo, dir.clone()] {
            let (tx, rx) = mpsc::channel();
            let p = path.clone();
            thread::spawn(move || tx.send(open_wav(&p).err()));
            let err = rx.recv_timeout(std::time::Duration::from_secs(2));
            let want = format!("bad_audio: not a regular file: {}", path.display());
            assert_eq!(err, Ok(Some(want)));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn feeder_delivers_all_samples_then_eof() {
        let path = wav("full", 16000, 4000);
        let msgs = feed(&path, false);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(sample_count(&msgs), 4000);
        assert_eq!(msgs.len(), 4); // 1600 + 1600 + 800 + Eof
        assert!(
            matches!(msgs.last(), Some(AudioMsg::Eof { gen: 7 })),
            "{msgs:?}"
        );
    }

    #[test]
    fn feeder_stops_when_flag_set() {
        let path = wav("stop", 16000, 16000);
        let msgs = feed(&path, true);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert_eq!(sample_count(&msgs), CHUNK);
    }

    #[test]
    fn feeder_truncated_file_fails() {
        let path = wav("trunc", 16000, 4000);
        let len = std::fs::metadata(&path).unwrap().len();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len - 4001)
            .unwrap();
        let msgs = feed(&path, false);
        std::fs::remove_file(&path).unwrap();
        let last = msgs.last();
        assert!(
            matches!(last, Some(AudioMsg::Failed { gen: 7, msg }) if msg.starts_with("bad_audio: ")),
            "{msgs:?}"
        );
    }
}
