use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::wait_hup;

#[test]
fn wait_hup_true_when_peer_dropped() {
    let (a, b) = UnixStream::pair().unwrap();
    drop(b);
    let t0 = Instant::now();
    assert!(wait_hup(a.as_raw_fd(), 100));
    assert!(t0.elapsed() < Duration::from_millis(100));
}

#[test]
fn wait_hup_false_after_timeout_when_peer_only_shuts_down_write() {
    let (a, b) = UnixStream::pair().unwrap();
    b.shutdown(Shutdown::Write).unwrap();
    let t0 = Instant::now();
    assert!(!wait_hup(a.as_raw_fd(), 100));
    assert!(t0.elapsed() >= Duration::from_millis(100));
}
