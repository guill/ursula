//! `FakeMic`: a throwaway headless PulseAudio whose only source is the fifo-fed `fakemic`
//! (plan "Fake mic harness"), and `Bg`, a deadline-bounded background child process.

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::cli::run_cli;
use crate::harness::{drain, lock, poll_until, scratch_dir};

/// 100 ms of 16 kHz s16le mono.
const TICK_BYTES: usize = 3200;

pub struct FakeMic {
    pub dir: PathBuf,
    pulse: Child,
    ffmpeg: Vec<Child>,
    feeders: Vec<JoinHandle<()>>,
}

impl FakeMic {
    /// `None` (after printing `skipped:`) without `URSULA_E2E_PULSE=1` or when pulseaudio
    /// cannot start (e.g. inside the Nix sandbox).
    pub fn start() -> Option<Self> {
        if std::env::var("URSULA_E2E_PULSE").as_deref() != Ok("1") {
            println!("skipped: URSULA_E2E_PULSE=1 is not set");
            return None;
        }
        let dir = scratch_dir();
        let d = dir.display();
        let spawned = Command::new("pulseaudio")
            .args(["-n", "--daemonize=no", "--exit-idle-time=-1", "--use-pid-file=no"])
            .args(["--disable-shm=yes", "-L"])
            .arg(format!(
                "module-native-protocol-unix socket={d}/pulse.sock auth-anonymous=1"
            ))
            .arg("-L")
            .arg(format!("module-pipe-source source_name=fakemic file={d}/mic.fifo format=s16le rate=16000 channels=1"))
            .envs([("HOME", &dir), ("XDG_RUNTIME_DIR", &dir), ("PULSE_RUNTIME_PATH", &dir)])
            .env("PULSE_SERVER", format!("unix:{d}/pulse.sock"))
            .env("PULSE_SOURCE", "fakemic")
            // Else it registers org.PulseAudio1 on the user's real session bus (or clashes).
            .env("DBUS_SESSION_BUS_ADDRESS", "disabled:")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut pulse = match spawned {
            Ok(child) => child,
            Err(e) => {
                println!("skipped: pulseaudio failed to start: {e}");
                let _ = fs::remove_dir_all(&dir);
                return None;
            }
        };
        let (log, _) = drain(pulse.stderr.take().expect("stderr"));
        let mut mic = Self {
            dir,
            pulse,
            ffmpeg: Vec::new(),
            feeders: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if mic.pactl("sources").contains("fakemic") {
                return Some(mic);
            }
            let exited = mic.pulse.try_wait().expect("try_wait").is_some();
            let log = String::from_utf8_lossy(&lock(&log)).into_owned();
            if exited {
                println!("skipped: pulseaudio failed to start: {log}");
                return None; // Drop reaps and cleans up.
            }
            assert!(
                Instant::now() < deadline,
                "fakemic not listed in 10 s: {log}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn server(&self) -> String {
        format!("unix:{}/pulse.sock", self.dir.display())
    }

    /// Daemon env per the plan: `PULSE_SERVER` and `PULSE_SOURCE`.
    pub fn env(&self) -> [(&'static str, String); 2] {
        [
            ("PULSE_SERVER", self.server()),
            ("PULSE_SOURCE", "fakemic".into()),
        ]
    }

    /// `pactl list short <what>` against this daemon only (deadline 5 s).
    pub fn pactl(&self, what: &str) -> String {
        let mut cmd = Command::new("pactl");
        cmd.args(["list", "short", what])
            .envs([("HOME", &self.dir), ("XDG_RUNTIME_DIR", &self.dir)])
            .env("PULSE_RUNTIME_PATH", &self.dir)
            .env("PULSE_SERVER", self.server());
        let out = run_cli(cmd, None, Duration::from_secs(5));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Polls `source-outputs` every 50 ms until a capture stream is listed (`open`) or none is.
    pub fn wait_stream(&self, open: bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.pactl("source-outputs").trim().is_empty() != open {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// `kill -<sig> <pulseaudio pid>`.
    pub fn signal(&self, sig: &str) {
        let mut cmd = Command::new("kill");
        cmd.arg(format!("-{sig}")).arg(self.pulse.id().to_string());
        let out = run_cli(cmd, None, Duration::from_secs(5));
        assert!(out.status.success(), "kill -{sig} failed: {out:?}");
    }

    /// Feeds `wav` into the fifo at real-time pace (`ffmpeg -re`, first `secs` seconds if
    /// given), then `zeros_ms` of explicit zero PCM, also paced. Returns at once.
    pub fn feed(&mut self, wav: Option<(&Path, Option<u32>)>, zeros_ms: u64) {
        let mut fifo = File::options()
            .write(true)
            .open(self.dir.join("mic.fifo"))
            .expect("open fifo");
        let pcm = wav.map(|(wav, secs)| {
            let mut cmd = Command::new("ffmpeg");
            cmd.args(["-nostdin", "-loglevel", "error", "-re", "-i"])
                .arg(wav)
                .args(
                    secs.map(|s| ["-t".to_owned(), s.to_string()])
                        .into_iter()
                        .flatten(),
                )
                .args(["-f", "s16le", "-ar", "16000", "-ac", "1", "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            let mut child = cmd.spawn().expect("spawn ffmpeg");
            let out = child.stdout.take().expect("ffmpeg stdout");
            self.ffmpeg.push(child);
            out
        });
        self.feeders.push(thread::spawn(move || {
            if let Some(mut pcm) = pcm {
                // EPIPE once pulseaudio is gone; that ends the feed.
                if io::copy(&mut pcm, &mut fifo).is_err() {
                    return;
                }
            }
            let tick = vec![0u8; TICK_BYTES];
            for _ in 0..zeros_ms.div_ceil(100) {
                if fifo.write_all(&tick).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }));
    }

    /// Waits until every feed so far has been written; panics after `timeout`.
    pub fn wait_fed(&mut self, timeout: Duration) {
        let ok = poll_until(timeout, || self.feeders.iter().all(JoinHandle::is_finished));
        assert!(ok, "feed not finished after {timeout:?}");
        for f in self.feeders.drain(..) {
            let _ = f.join();
        }
        for mut c in self.ffmpeg.drain(..) {
            let status = c.wait().expect("wait ffmpeg");
            assert!(status.success(), "ffmpeg: {status}");
        }
    }
}

impl Drop for FakeMic {
    fn drop(&mut self) {
        if matches!(self.pulse.try_wait(), Ok(None)) {
            let pid = self.pulse.id().to_string();
            for sig in ["-CONT", "-KILL"] {
                let _ = Command::new("kill").args([sig, &pid]).status();
            }
        }
        let _ = self.pulse.wait();
        for c in &mut self.ffmpeg {
            let _ = c.kill();
            let _ = c.wait();
        }
        // ffmpeg is dead and the fifo has no reader: every feeder write now fails.
        for f in self.feeders.drain(..) {
            let _ = f.join();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A background child (own process group) with stdout/stderr drained concurrently.
pub struct Bg {
    child: Child,
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
    drains: [JoinHandle<()>; 2],
}

impl Bg {
    pub fn spawn(mut cmd: Command) -> Self {
        cmd.process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn background child");
        let (out, out_t) = drain(child.stdout.take().expect("stdout"));
        let (err, err_t) = drain(child.stderr.take().expect("stderr"));
        Self {
            child,
            out,
            err,
            drains: [out_t, err_t],
        }
    }

    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub fn kill9(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Waits for exit and drained pipes; on timeout kills, reaps and panics with the output.
    pub fn wait(mut self, timeout: Duration) -> Output {
        let deadline = Instant::now() + timeout;
        let mut status = None;
        while Instant::now() < deadline {
            status = status.or_else(|| self.child.try_wait().expect("try_wait"));
            if let Some(status) = status.filter(|_| self.drains.iter().all(JoinHandle::is_finished))
            {
                return Output {
                    status,
                    stdout: lock(&self.out).clone(),
                    stderr: lock(&self.err).clone(),
                };
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.kill9();
        let (out, err) = (lock(&self.out).clone(), lock(&self.err).clone());
        panic!(
            "background child timed out after {timeout:?}; stdout {:?} stderr {:?}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        );
    }
}

impl Drop for Bg {
    fn drop(&mut self) {
        self.kill9();
    }
}
