//! Linux-only raw syscalls, declared by hand (no libc crate): kill(2) and poll(2).

use std::os::fd::RawFd;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
}

/// SIGKILL every process in group `pgid`. 0 and 1 are refused: `kill(0)` would hit our
/// own group and `kill(-1)` every process we may signal.
pub fn kill_group(pgid: u32) {
    if pgid > 1 {
        // SAFETY: kill(2) takes only integers and touches no memory of ours.
        unsafe { kill(-(pgid as i32), 9) };
    }
}

/// True if `fd` reports POLLHUP, POLLERR or POLLNVAL within `timeout_ms`.
pub fn wait_hup(fd: RawFd, timeout_ms: i32) -> bool {
    let mut p = PollFd {
        fd,
        events: 0,
        revents: 0,
    };
    // SAFETY: `p` is a live, exclusively borrowed PollFd and nfds is 1. A failed poll
    // leaves `revents` at 0, which reads as "no hangup".
    unsafe { poll(&mut p, 1, timeout_ms) };
    p.revents & (0x10 | 0x8 | 0x20) != 0
}

#[cfg(test)]
mod tests;
