//! End-to-end tests against the real `ursula` binary (plan todo 12).
//!
//! Model-backed tests need `URSULA_E2E_MODEL_DIR` and `URSULA_E2E_VAD`; without them they
//! print `skipped: <reason>` and pass. `harness_timeout` and `cli_smoke` need nothing.
//! The fake-mic `mic_` tests (todo 13) additionally need `URSULA_E2E_PULSE=1`.
//! `bench` (todo 14) is ignored; run it with `--ignored --nocapture`.

#[path = "e2e/bench.rs"]
mod bench;
#[path = "e2e/cli.rs"]
mod cli;
#[path = "e2e/control.rs"]
mod control;
#[path = "e2e/fakemic.rs"]
mod fakemic;
#[path = "e2e/file.rs"]
mod file;
#[path = "e2e/harness.rs"]
mod harness;
#[path = "e2e/load.rs"]
mod load;
#[path = "e2e/mic.rs"]
mod mic;
#[path = "e2e/push.rs"]
mod push;
#[path = "e2e/text.rs"]
mod text;
#[path = "e2e/wire.rs"]
mod wire;
