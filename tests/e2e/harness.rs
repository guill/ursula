//! Daemon fixture, config merging and process helpers. Every blocking call has a deadline:
//! sockets get 5 s read/write timeouts and event reads use absolute deadlines (`wire`).

use std::fs;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use toml::Table;

pub use crate::wire::{audio_lines, expect_event, expect_event_any, push_start, Conn, IO_TIMEOUT};

/// Generous bound for a whole decode of 0.wav on a loaded machine.
pub const DECODE: Duration = Duration::from_secs(60);

/// A model daemon loads ~1 GB and decodes on every core, so model tests run one at a
/// time: timing assertions then measure the daemon, not its neighbours.
static SERIAL: Mutex<()> = Mutex::new(());

/// A fresh directory under `temp_dir()`, unique per process and call.
pub fn scratch_dir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ursula-e2e-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Copies `r` into a shared buffer from a detached thread until EOF or error.
pub fn drain(mut r: impl Read + Send + 'static) -> (Arc<Mutex<Vec<u8>>>, JoinHandle<()>) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buf);
    let handle = thread::spawn(move || {
        let mut chunk = [0; 8192];
        while let Ok(n @ 1..) = r.read(&mut chunk) {
            lock(&sink).extend_from_slice(&chunk[..n]);
        }
    });
    (buf, handle)
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Deep merge: tables merge key by key, any other value replaces.
pub fn merge(base: &mut Table, over: Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Parses a TOML snippet; for per-test overrides.
pub fn tbl(text: &str) -> Table {
    toml::from_str(text).expect("valid TOML override")
}

const BASE: &str = r#"
[postprocessors.upper]
command = ["sh", "-c", "cat >/dev/null; echo '{\"text\":\"POST\"}'"]
[postprocessors.bad]
command = ["false"]
"#;

pub struct Daemon {
    child: Child,
    log: Arc<Mutex<Vec<u8>>>,
    pub dir: PathBuf,
    pub socket: PathBuf,
    /// `<model dir>/test_wavs/0.wav`.
    pub wav: PathBuf,
    _serial: MutexGuard<'static, ()>,
}

impl Daemon {
    /// `None` (after printing `skipped:`) when the model env vars are unset.
    pub fn start(overrides: Table) -> Option<Self> {
        Self::start_with_env(overrides, &[])
    }

    /// Like [`Daemon::start`], with extra environment variables for the daemon process.
    pub fn start_with_env(overrides: Table, envs: &[(&str, &str)]) -> Option<Self> {
        let var = |k| std::env::var(k).ok().filter(|v| !v.is_empty());
        let (Some(model), Some(vad)) = (var("URSULA_E2E_MODEL_DIR"), var("URSULA_E2E_VAD")) else {
            println!("skipped: URSULA_E2E_MODEL_DIR and URSULA_E2E_VAD must be set");
            return None;
        };
        let serial = lock(&SERIAL);
        let mut cfg = tbl(BASE);
        merge(
            &mut cfg,
            tbl(&format!("model.dir = {model:?}\nvad.model = {vad:?}")),
        );
        merge(&mut cfg, overrides);
        let dir = scratch_dir();
        let config = dir.join("config.toml");
        fs::write(&config, toml::to_string(&cfg).expect("serialize config")).expect("write");
        let socket = dir.join("s.sock");
        let mut child = Command::new(env!("CARGO_BIN_EXE_ursula"))
            .arg("serve")
            .arg("--socket")
            .arg(&socket)
            .arg("--config")
            .arg(&config)
            .envs(envs.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ursula serve");
        let (log, _) = drain(child.stderr.take().expect("stderr"));
        let daemon = Self {
            child,
            log,
            wav: Path::new(&model).join("test_wavs/0.wav"),
            dir,
            socket,
            _serial: serial,
        };
        daemon.wait_ready();
        Some(daemon)
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while UnixStream::connect(&self.socket).is_err() {
            assert!(
                Instant::now() < deadline,
                "daemon not ready: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn log(&self) -> String {
        String::from_utf8_lossy(&lock(&self.log)).into_owned()
    }

    /// Polls the daemon stderr for `needle` until `timeout`.
    pub fn wait_log(&self, needle: &str, timeout: Duration) -> bool {
        poll_until(timeout, || self.log().contains(needle))
    }

    pub fn connect(&self) -> Conn {
        Conn::new(UnixStream::connect(&self.socket).expect("connect"))
    }

    /// One `status` round trip on a fresh connection.
    pub fn status(&self) -> Value {
        let mut c = self.connect();
        c.send(&json!({"op": "status"}));
        c.expect("status", IO_TIMEOUT)
    }

    /// Thread count of the daemon process.
    pub fn threads(&self) -> usize {
        let task = format!("/proc/{}/task", self.child.id());
        fs::read_dir(task).expect("read /proc task").count()
    }

    /// Polls until the daemon is back to at most `limit` threads.
    pub fn threads_settle(&self, limit: usize, timeout: Duration) {
        let ok = poll_until(timeout, || self.threads() <= limit);
        assert!(ok, "threads {} > {limit} after {timeout:?}", self.threads());
    }

    /// `ursula --socket <ours> <args>`, for [`crate::cli::run_cli`].
    pub fn cli(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ursula"));
        cmd.arg("--socket").arg(&self.socket).args(args);
        cmd
    }

    pub fn pcm(&self) -> Vec<i16> {
        let mut r = hound::WavReader::open(&self.wav).expect("open 0.wav");
        r.samples::<i16>().map(|s| s.expect("sample")).collect()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub fn poll_until(timeout: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}
