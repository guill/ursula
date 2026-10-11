//! Mic capture: a detached libpulse record-stream reader (DESIGN.md "Mic lifecycle").
//! The stream lives only as long as the reader thread; it is never kept open between sessions.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;

use libpulse_binding::def::BufferAttr;
use libpulse_binding::error::PAErr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

use crate::audio::{self, AudioMsg, AudioSink};

/// Monotonic milliseconds since the first call (the daemon calls it at start-up). Every `*_ms`
/// value in the daemon, including the mic's `last_data`, uses this clock, never wall-clock time.
pub fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Opens the default (or `PULSE_SOURCE`) capture device on `server` (`None` = default server)
/// and streams 100 ms `Samples` until `stop` is set, the sink is gone, or the stream fails.
/// `last_data` gets `now_ms()` after every successful read. The thread is detached; its very
/// last act on every path is `send(MicExited{gen})`, after the stream has been closed.
// simplification: pa_simple_read cannot be interrupted; a stalled reader stays blocked until PulseAudio recovers or dies. The controller refuses new mic sessions while it is alive (at most 1). Upgrade: libpulse async API with a cancellable mainloop.
pub fn spawn_mic(
    server: Option<String>,
    gen: u64,
    send: AudioSink,
    stop: Arc<AtomicBool>,
    last_data: Arc<AtomicU64>,
) {
    thread::spawn(move || {
        let reader = Reader { gen, send };
        reader.run(server.as_deref(), &stop, &last_data);
        // `reader` drops here (also when unwinding): the stream is already closed, MicExited goes out.
    });
}

/// Owns the sink so that dropping it sends `MicExited` unconditionally.
struct Reader {
    gen: u64,
    send: AudioSink,
}

impl Drop for Reader {
    fn drop(&mut self) {
        (self.send)(AudioMsg::MicExited { gen: self.gen });
    }
}

impl Reader {
    fn run(&self, server: Option<&str>, stop: &AtomicBool, last_data: &AtomicU64) {
        let gen = self.gen;
        let failed = |e: PAErr| AudioMsg::Failed {
            gen,
            msg: format!("audio_unavailable: {e}"),
        };
        let spec = Spec {
            format: Format::S16le,
            channels: 1,
            rate: 16000,
        };
        // BufferAttr::default() is all zeros; u32::MAX means "server default".
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: 3200,
        };
        let opened = Simple::new(
            server,
            "ursula",
            Direction::Record,
            None,
            "capture",
            &spec,
            None,
            Some(&attr),
        );
        let stream = match opened {
            Ok(stream) => stream,
            Err(e) => {
                (self.send)(failed(e));
                return;
            }
        };
        if stop.load(Ordering::Relaxed) || !(self.send)(AudioMsg::Opened { gen }) {
            return;
        }
        let mut buf = [0u8; 3200]; // 100 ms
        while !stop.load(Ordering::Relaxed) {
            let msg = match stream.read(&mut buf) {
                Err(e) => failed(e),
                Ok(()) => {
                    last_data.store(now_ms(), Ordering::Relaxed);
                    match audio::s16le_to_f32(&buf) {
                        Ok(data) => AudioMsg::Samples { gen, data },
                        Err(msg) => AudioMsg::Failed { gen, msg }, // unreachable: even length
                    }
                }
            };
            let more = matches!(msg, AudioMsg::Samples { .. });
            if !(self.send)(msg) || !more {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    fn spawn(server: String, stop: &Arc<AtomicBool>, last: &Arc<AtomicU64>) -> Receiver<AudioMsg> {
        let (tx, rx) = mpsc::channel();
        let sink: AudioSink = Box::new(move |m| tx.send(m).is_ok());
        spawn_mic(Some(server), 9, sink, Arc::clone(stop), Arc::clone(last));
        rx
    }

    /// Receives up to and including the first message matching `want`; panics on the deadline.
    fn recv_until(
        rx: &Receiver<AudioMsg>,
        secs: u64,
        want: fn(&AudioMsg) -> bool,
    ) -> Vec<AudioMsg> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut got = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let m = rx
                .recv_timeout(left)
                .unwrap_or_else(|e| panic!("{e} after {got:?}"));
            let done = want(&m);
            got.push(m);
            if done {
                return got;
            }
        }
    }

    fn assert_closed(rx: &Receiver<AudioMsg>) {
        let next = rx.recv_timeout(Duration::from_secs(1));
        assert!(next.is_err_and(|e| e == mpsc::RecvTimeoutError::Disconnected));
    }

    #[test]
    fn unreachable_server_fails_then_exits() {
        let (stop, last) = (Arc::default(), Arc::default());
        let rx = spawn("unix:/nonexistent".into(), &stop, &last);
        let msgs = recv_until(&rx, 3, |m| matches!(m, AudioMsg::MicExited { gen: 9 }));
        assert!(
            matches!(&msgs[..], [AudioMsg::Failed { gen: 9, msg }, AudioMsg::MicExited { gen: 9 }]
                if msg.starts_with("audio_unavailable: ")),
            "{msgs:?}"
        );
        assert_closed(&rx);
    }

    /// Throwaway headless PulseAudio whose only source is the fifo-fed `fakemic` (plan "Fake mic
    /// harness"). Drop kills and reaps it and deletes its directory.
    struct Pulse {
        dir: PathBuf,
        child: Child,
    }

    impl Pulse {
        fn start(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("ursula-mic-{}-{name}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let d = dir.display();
            let child = Command::new("pulseaudio")
                .args(["-n", "--daemonize=no", "--exit-idle-time=-1", "--use-pid-file=no"])
                .args(["--disable-shm=yes", "-L"])
                .arg(format!("module-native-protocol-unix socket={d}/pulse.sock auth-anonymous=1"))
                .arg("-L")
                .arg(format!("module-pipe-source source_name=fakemic file={d}/mic.fifo format=s16le rate=16000 channels=1"))
                .envs([("HOME", &dir), ("XDG_RUNTIME_DIR", &dir), ("PULSE_RUNTIME_PATH", &dir)])
                .env("DBUS_SESSION_BUS_ADDRESS", "disabled:") // else 2 daemons clash on D-Bus
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let pulse = Pulse { dir, child };
            pulse.wait_for(10, |p| p.pactl("sources").contains("fakemic"));
            pulse
        }

        fn server(&self) -> String {
            format!("unix:{}/pulse.sock", self.dir.display())
        }

        /// `pactl list short <what>` against this daemon.
        fn pactl(&self, what: &str) -> String {
            let out = Command::new("pactl")
                .args(["list", "short", what])
                .env("HOME", &self.dir)
                .env("PULSE_SERVER", self.server())
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        }

        fn wait_for(&self, secs: u64, ok: fn(&Pulse) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(secs);
            while !ok(self) {
                assert!(Instant::now() < deadline, "pulse condition timed out");
                thread::sleep(Duration::from_millis(100));
            }
        }

        /// Writes 0.5-amplitude PCM into the fifo until the daemon dies (EPIPE).
        fn feed(&self) {
            let mut fifo = File::options()
                .write(true)
                .open(self.dir.join("mic.fifo"))
                .unwrap();
            let pcm = 16384i16.to_le_bytes().repeat(1600);
            thread::spawn(move || {
                while fifo.write_all(&pcm).is_ok() {
                    thread::sleep(Duration::from_millis(50));
                }
            });
        }
    }

    impl Drop for Pulse {
        fn drop(&mut self) {
            let _ = self.child.kill(); // already dead in the hard-error test
            let _ = self.child.wait();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Starts a fed daemon and a reader; returns once the reader has delivered real samples.
    fn streaming(name: &str) -> Option<(Pulse, Receiver<AudioMsg>, Arc<AtomicBool>)> {
        if std::env::var_os("URSULA_E2E_PULSE").is_none() {
            println!("skipped: URSULA_E2E_PULSE is not set");
            return None;
        }
        let pulse = Pulse::start(name);
        pulse.feed();
        let (stop, last) = (Arc::default(), Arc::new(AtomicU64::new(u64::MAX)));
        let rx = spawn(pulse.server(), &stop, &last);
        let msgs = recv_until(&rx, 5, |m| matches!(m, AudioMsg::Samples { .. }));
        assert!(
            matches!(&msgs[..], [AudioMsg::Opened { gen: 9 }, AudioMsg::Samples { gen: 9, data }]
                if data.len() == 1600 && data.iter().all(|&s| s == 0.5)),
            "{msgs:?}"
        );
        assert!(last.load(Ordering::Relaxed) <= now_ms());
        assert_eq!(pulse.pactl("source-outputs").lines().count(), 1);
        Some((pulse, rx, stop))
    }

    #[test]
    fn real_pulse_streams_until_stop_then_closes_the_stream() {
        let Some((pulse, rx, stop)) = streaming("stop") else {
            return;
        };
        stop.store(true, Ordering::Relaxed);
        let msgs = recv_until(&rx, 3, |m| matches!(m, AudioMsg::MicExited { gen: 9 }));
        assert!(
            matches!(&msgs[..], [samples @ .., AudioMsg::MicExited { .. }]
                if samples.iter().all(|m| matches!(m, AudioMsg::Samples { .. }))),
            "{msgs:?}"
        );
        assert_closed(&rx);
        pulse.wait_for(2, |p| p.pactl("source-outputs").is_empty());
    }

    #[test]
    fn real_pulse_killed_daemon_fails_then_exits() {
        let Some((mut pulse, rx, _stop)) = streaming("kill") else {
            return;
        };
        pulse.child.kill().unwrap();
        let msgs = recv_until(&rx, 3, |m| matches!(m, AudioMsg::MicExited { gen: 9 }));
        assert!(
            matches!(&msgs[..], [.., AudioMsg::Failed { gen: 9, msg }, AudioMsg::MicExited { .. }]
                if msg.starts_with("audio_unavailable: ")),
            "{msgs:?}"
        );
        assert_closed(&rx);
    }
}
