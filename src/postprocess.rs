//! External post-processor chain (DESIGN.md "Post-processor protocol"). A failing step is
//! skipped with a warning and never loses the text produced so far.

use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::sys;

const MAX_OUTPUT: u64 = 1 << 20;
const POLL: Duration = Duration::from_millis(20);

/// Local stand-in for the `[postprocessors.<name>]` config entry; same name and fields.
pub struct PostprocessorCfg {
    pub command: Vec<String>,
    pub timeout_ms: u64,
}

/// Local stand-in for the protocol segment; same name and fields.
#[derive(Serialize)]
pub struct Seg {
    pub text: String,
    pub start: f32,
    pub end: f32,
}

pub struct PostJob {
    pub text: String,
    pub context: Option<String>,
    pub hotwords: Vec<String>,
    pub segments: Vec<Seg>,
    pub chain: Vec<(String, PostprocessorCfg)>,
}

#[derive(Serialize)]
struct Input<'a> {
    text: &'a str,
    context: Option<&'a str>,
    hotwords: &'a [String],
    segments: &'a [Seg],
}

#[derive(Deserialize)]
struct Output {
    text: String,
}

enum Step {
    Text(String),
    Warn(String),
    Cancelled,
}

/// Runs every step in order; returns the last good text and one warning per skipped step.
pub fn run_chain(job: PostJob, cancel: &AtomicBool) -> (String, Vec<String>) {
    let mut text = job.text;
    let mut warnings = Vec::new();
    for (name, cfg) in &job.chain {
        let input = Input {
            text: &text,
            context: job.context.as_deref(),
            hotwords: &job.hotwords,
            segments: &job.segments,
        };
        // Strings, string slices and f32s always serialize (NaN becomes null).
        let input = serde_json::to_vec(&input).unwrap_or_default();
        match run_step(cfg, input, cancel) {
            Step::Text(t) => text = t,
            Step::Warn(w) => warnings.push(format!("postprocessor {name} {w}")),
            Step::Cancelled => break,
        }
    }
    (text, warnings)
}

fn run_step(cfg: &PostprocessorCfg, input: Vec<u8>, cancel: &AtomicBool) -> Step {
    let Some((program, args)) = cfg.command.split_first() else {
        return Step::Warn("could not start: empty command".into());
    };
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return Step::Warn(format!("could not start: {e}")),
    };
    let pgid = child.id();
    let deadline = Instant::now() + Duration::from_millis(cfg.timeout_ms);
    // Helper threads are never joined: killing the group closes their pipes.
    if let Some(mut stdin) = child.stdin.take() {
        thread::spawn(move || {
            let _ = stdin.write_all(&input); // a child that never reads gives EPIPE; fine
        });
    }
    let (tx, rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        thread::spawn(move || {
            let mut out = Vec::new();
            // The pipe closes at the end of this statement, so an over-long writer gets
            // SIGPIPE instead of blocking until the deadline.
            let _ = stdout.take(MAX_OUTPUT + 1).read_to_end(&mut out);
            let _ = tx.send(out);
        });
    }
    let mut status = None;
    let mut out = None;
    loop {
        if status.is_none() {
            status = child.try_wait().ok().flatten();
            if status.is_some() {
                // The leader is done: kill background descendants still holding stdout.
                sys::kill_group(pgid);
            }
        }
        out = out.or_else(|| rx.try_recv().ok());
        if let (Some(status), Some(out)) = (status, &out) {
            return verdict(status, out);
        }
        let cancelled = cancel.load(Ordering::Relaxed);
        if cancelled || Instant::now() >= deadline {
            sys::kill_group(pgid);
            let _ = child.wait();
            if cancelled {
                return Step::Cancelled;
            }
            return Step::Warn(format!("timed out after {} ms", cfg.timeout_ms));
        }
        thread::sleep(POLL);
    }
}

fn verdict(status: ExitStatus, out: &[u8]) -> Step {
    // Checked before the exit status: overflow makes the child die of SIGPIPE.
    if out.len() as u64 > MAX_OUTPUT {
        return Step::Warn("invalid output".into());
    }
    if !status.success() {
        let code = status.code().map_or_else(
            || format!("signal {}", status.signal().unwrap_or(0)),
            |c| c.to_string(),
        );
        return Step::Warn(format!("failed: exit {code}"));
    }
    match serde_json::from_slice::<Output>(out) {
        Ok(o) => Step::Text(o.text),
        Err(_) => Step::Warn("invalid output".into()),
    }
}

#[cfg(test)]
mod tests;
