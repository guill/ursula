# Ursula Design

This document is meant to be the design proposal for Ursula -- a service intended to make voice transcription easily and efficiently supported on Linux systems (including WSL2).

## Problem Statement
As AI becomes more proficient, being able to interact with it via voice is becoming more important. People are looking for things like:

1. Integration with NeoVim to type what I say when a keybinding is pressed.
2. Adding a WM-level key binding that records what I say and feeds it to a custom AI agent script.
3. Adding a WM-level key binding that records what I say and types it in the active window.

The goal of Ursula is not to directly implement these things -- it's to provide the API and bindings that make it easy for someone to add voice support to whatever plugin-enabled app they want to.

### Existing Alternatives

One alternative used for NeoVim today is vocal.nvim. It works by invoking a Python script which records an audio clip and then loads a whisper model to transcribe it before feeding the info back to NeoVim. There are a couple issues with this method:

1. We have to reload the voice model every time.
2. We don't start transcribing until the entire audio clip is recorded.
3. It's fairly specific to NeoVim and we'd load up an entire copy of it to run transcription for another application.

### Design Goals
The following are design goals, but *not* necessarily hard requirements. If we can achieve all of them we should. If we can't, we should discuss which are worth cutting.

- Accuracy
    - Transcription should be high accuracy
    - We should be able to feed additional context to the service to help it transcribe accurately when the model used allows this.
- Portability
    - We should support CPU-only models, though may also want to add stronger GPU-powered models
    - The API should be provided in a way that's relatively easy to integrate with different applications and languages (e.g. an HTTP API, named pipes, or something similar). For example, I would like the NeoVim plugin using it to work entirely in Lua without any necessary Python subshells.
    - First-class support for Nix and NixOS. Include a flake for both development, running on demand, and running as a service.
- Speed
    - We should be able to keep the model loaded so that transcription is very fast.
    - We should support beginning to transcribe before the speaker is finished speaking (e.g. by chunking). API should support both the option of streaming results and getting all text at once when it's done.
- Extendability
    - The system should be extendable. E.g. someone might want to have a fast LLM like Haiku clean up the text/punctuation on finish.
    - The system shouldn't be *too* tied to a specific model. In particular, we should be able to change the model without changing any of the integrations that use our API in the future. We *don't* necessarily need to support multiple models at once.

### Suggestions
These are suggestions based on my research, but if you have better ideas you should push back.
- `Parakeet TDT 0.6B v2 (English)` seems like a strong contender as the model.
- `Nemotron Streaming 0.6B` may have better streaming support, but also appears to be less accurate.
- Running as a persistent service is likely the best way to do this, though we should be conscious of memory usage.
- We should evaluate whether Ursula itself should handle recording from a microphone or if there's an easy way for different languages to stream audio data from a microphone to Ursula.
- `sherpa-onnx` may be worth looking at, either to power Ursula or just to get ideas on implementation from
- To allows for 'streaming' in offline models, one option is using something like Silero VAD to find speech segments and then run the offline model on each segment.

## Decisions

| Topic | Decision |
|---|---|
| Model (v1) | Parakeet TDT 0.6B v2, int8 ONNX (`sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8`, ~480 MB). Config-selectable so v3 (multilingual) is a drop-in. |
| Streaming strategy | Silero VAD splits audio into speech segments; each finished segment is decoded by the offline model. Nemotron streaming (WER 6.93 vs 6.05 at best chunk size) is a possible future engine behind the same API. |
| Runtime | sherpa-onnx (in nixpkgs, 1.13.x) via the official `sherpa-onnx` Rust crate. |
| Language | Rust. One binary, `ursula`, which is both the daemon (`ursula serve`) and the CLI client. |
| Audio capture | Ursula captures from the mic itself via **libpulse** (works on PulseAudio, PipeWire via pipewire-pulse, and WSLg). The mic is opened **only while a session is active**. Pushed PCM and WAV files are secondary inputs. |
| Transport | Newline-delimited JSON (NDJSON) over a Unix socket at `$XDG_RUNTIME_DIR/ursula.sock`, mode `0600`. File permissions are the auth. No TCP in v1. |
| Concurrency | One session at a time, globally. A second `start` gets a `busy` error. |
| Streamed output | Final (never-revised) `segment` events, then `done`. A `partial` event is reserved in the schema for a future true-streaming engine. |
| Context | Per-request `hotwords` (list) and `context` (free text), plus a global hotword list in config. Hotwords bias the model only when supported and enabled; everything is always passed to post-processors. |
| Extensibility | Named post-processors defined in config as external commands, JSON on stdin and stdout, chained in order, applied to final text only. |
| Memory | Model loaded at startup and kept resident. To free memory, stop the service. |
| Model delivery | Nix fixed-output derivations, pinned by hash. Non-Nix users point config at a directory (a download script is provided). |
| Service | systemd **user** service, which can reach the user's audio session and `$XDG_RUNTIME_DIR`. |
| Performance target | None fixed. Stop-to-text latency and resident memory are recorded under [Measurements](#measurements). |

## Measurements

Measured on 2026-10-11 via the e2e bench (`cargo test --test e2e bench -- --ignored --nocapture`): a debug build of ursula, with inference running in the release-built sherpa-onnx/onnxruntime libraries. Each of 5 runs pushes `test_wavs/0.wav` (7.4 s) at real-time pace (one 100 ms chunk every 100 ms) with `stream: false` and no post-processors, then sends `stop`. Latency is wall time from `stop` to `done`. Memory comes from `/proc/<pid>/status` in MiB: idle RSS once the socket is ready, peak `VmHWM` after the runs. The desktop was otherwise lightly loaded (load average ~2).

| Item | Value |
|---|---|
| CPU | AMD Ryzen 7 7700X 8-Core Processor |
| Threads (`model.threads`) | 4 |
| Model | `parakeet-tdt-0.6b-v2-int8` |
| Stop-to-text, median / min / max | 108 / 108 / 115 ms |
| Idle RSS | 1097 MiB |
| Peak RSS (`VmHWM`) | 1151 MiB |

## Architecture

```
            ┌──────────────── ursula serve ─────────────────────────────┐
 client ──► │ socket thread(s) ──► session controller (single session)  │
 (NDJSON)   │                         │                                 │
            │      AudioSource ───────┤  mic (libpulse) | push | WAV    │
            │                         ▼                                 │
            │      Engine (trait) ── accept(samples) / finish()         │
            │        └─ VadOfflineEngine: Silero VAD → Parakeet TDT     │
            │                         │  segment events                 │
            │                         ▼                                 │
            │      post-processor chain (external commands, on done)    │
            └──────────────────────────┬────────────────────────────────┘
                                       ▼
                         events → owning client connection
```

- **Engine** is the single seam for model independence. Everything downstream consumes its events.
  ```rust
  trait Engine {
      fn begin(&mut self, hotwords: &[String]);      // new session
      fn accept(&mut self, samples: &[f32]) -> Vec<EngineEvent>; // 16 kHz mono f32
      fn finish(&mut self) -> Vec<EngineEvent>;      // flush VAD, decode tail
      fn in_speech(&self) -> bool;                   // last fed window flagged as speech
      fn capabilities(&self) -> Capabilities;       // e.g. hotwords support
  }
  enum EngineEvent { Segment { text: String, start: f32, end: f32 }, Partial { text: String } }
  ```
  `VadOfflineEngine` is the only v1 implementation. A Nemotron `OnlineRecognizer` engine would emit `Partial` and `Segment` events without any API change.
- **Threads:** one controller thread owns the engine (the model is never shared) and the session state machine. Each connection has a reader thread and a writer thread. Pushed audio arrives as `audio` requests through the connection reader and is decoded by the controller; feeder threads exist only for `file` sources, and the mic capture thread feeds `mic` sessions. Post-processors run on their own threads. Supervision ticks (open timeout, stall detection, mic worker) run inside the controller loop, every 100 ms of idle time and after every input; no tick messages are queued. Everything uses std threads (`std::os::unix::net`); there are too few clients to justify an async runtime.
- **Request ordering:** requests from one connection are processed strictly in the order sent, interleaved with feeder audio through ONE bounded ingress FIFO. Replies to lines that fail to parse (invalid JSON or UTF-8, unknown `op`, line too long) are, like `status`, sent directly by the connection's reader and are not ordered relative to that connection's earlier queued requests. Pushed audio therefore always reaches the engine before a later `stop` from the same connection.
- **Session states:** `Idle`, `Opening` (mic only, before the stream is ready), `Recording`, `Finishing` (post-processors running). `status.busy` is true in every state except `Idle`. A session has **exactly one terminal event**: `done`, `cancelled`, or an in-session `error`. After it, late input for that session (samples, EOF, failure, post-processor result) is ignored.

  | State | `stop` | `cancel` |
  |---|---|---|
  | Idle | `error no_session` | `error no_session` |
  | Opening | `ack`, then `cancelled` to the owner | `ack`, then `cancelled` to the owner |
  | Recording | `ack`, finalize, then `done` | `ack`, then `cancelled` |
  | Finishing | `ack` (idempotent) | `ack`, kills the running post-processor, `cancelled`, no `done` |

- **Connection teardown:** a *half-close* (read EOF, `shutdown(SHUT_WR)`) means "no more requests" and does **not** cancel. An idle connection is flushed and closed; an owner's session runs to its terminal event, which is delivered before the connection closes. A *full close* (POLLHUP), a write error, or a slow reader whose 1024-event outbound queue is full cancels the owned session silently and closes the connection. A duplicated descriptor held by a child process keeps the peer alive. This lets `printf ... | ursula raw` finish finite sessions, while a killed `ursula record` still closes the mic within about 200 ms.
- **Responsiveness under load** (design choices, not real-time guarantees): `status` is answered out-of-band by the connection's reader from a shared snapshot, so it is not ordered against that connection's earlier requests. A `cancel`, or a full close of the owner, sets a per-session hint that the controller checks before every 512-sample step; queued audio of that session is then dropped without decoding. A `cancel` queued behind blocked audio on the same connection is head-of-line blocked, so clients should cancel from a separate connection (as `ursula cancel` does). `stop` is ordered behind its own connection's queued audio.
- **Mic lifecycle:** open the PulseAudio record stream on `start`, close it on `stop`/`cancel`/`done`. If the stream errors or delivers no data for 2 s, end the session with `error: audio_unavailable` and keep the daemon running. This matters for the WSLg regressions noted below. After a stall the capture thread may be stuck in a blocking read; new mic sessions are refused (`audio_unavailable`) until it exits, so there is **at most one live mic reader** at any time.
- **Session ownership:** the session's events go to the connection that sent `start`. `stop` and `cancel` are accepted from **any** connection, because there is only one session. Both take an optional `"session"`; if it doesn't match the active session the reply is `error no_session`. This lets a WM toggle keybinding run as two separate processes (see CLI). If the owning connection drops, the session is cancelled so the mic is never left open.

## API

Transport: connect to `$XDG_RUNTIME_DIR/ursula.sock`. Every message is one JSON object per line. Requests may carry an optional `"id"`, echoed as follows:
- `error`, `ack` and `status` replies carry the `id` of the request they answer.
- All lifecycle events of a session (`started`, `segment`, `done`, `cancelled`, and in-session `error`) carry the `id` of its `start` request, if it had one.

Limits and bad requests: a request line may be at most 1 MiB (excluding the `\n`). A longer line gets `error bad_request "line too long"` and the connection is closed, which cancels any session it owns. Invalid JSON or an unknown `op` gets `bad_request` and the connection stays open. For `audio`: from a non-owner → `bad_request`; with no session → `no_session`; for a non-push session → `bad_request`; bad base64 or an odd byte count → a session-ending `error bad_audio`.

### Requests (client → daemon)

**`start`**: begin a session.
```json
{"op": "start",
 "source": "mic",               // "mic" | "push" | {"file": "/abs/path.wav"}
 "stream": true,                // emit segment events (default false: only done)
 "hotwords": ["NixOS", "sherpa-onnx"],
 "context": "Editing a Rust file about audio capture.",
 "postprocess": ["cleanup"],    // names from config, run in order
  "max_ms": 120000,              // hard cap; default from config
  "silence_stop_ms": 1500}       // optional: auto-stop after trailing silence
```

- `max_ms` is the duration of *accepted audio*, counted in samples, not wall time. The effective cap is `min(request.max_ms, config.session.max_ms)`. Samples past the cap are dropped and the session finishes with `reason: max_duration`. `max_ms` ≤ 0 is `bad_request`.
- `silence_stop_ms` is measured in sample time and applies to all sources. Audio is split into 512-sample steps whatever the wire chunking, and the session finishes with `reason: silence` once at least one speech window has been seen and the samples since the last speech window reach `silence_stop_ms`. The VAD's own `min_silence_ms` hang-over is included in that measurement. It never fires before the first speech, and results agree within ±512 samples across chunkings.
- `started` readiness: for `mic`, a session id is reserved and the state is `Opening` (busy) until the PulseAudio stream is open; only then is `started` sent. If opening fails or takes more than 2 s, the session ends with a terminal `error audio_unavailable` (with `session`) and `started` is never sent. File and push sessions send `started` right after validation.

**`audio`**: only for `source: "push"`. The payload is base64-encoded **16 kHz mono s16le** PCM.
```json
{"op": "audio", "data": "<base64>"}
```

**`stop`**: stop capturing and finalize (decode the tail, run post-processors, send `done`).
**`cancel`**: discard. No transcription is returned.
**`status`**: returns daemon state.
```json
{"op": "stop"}   {"op": "cancel"}   {"op": "status"}
{"op": "stop", "session": "s-17"}   // optional target; mismatch → error no_session
```

`stop` and `cancel` are answered with an `ack` event to the sender, then the session's terminal event goes to its owner (if the sender is the owner it receives both, `ack` first).

For `{"file": ...}` sources, the session ends on its own at end of file. `file` sources must be regular files (otherwise `bad_audio`). v1 accepts only 16 kHz mono 16-bit PCM WAV and rejects anything else with a `bad_audio` error that suggests an `ffmpeg` command. Push and file inputs feed audio faster than real time, so file transcription is fast.

### Events (daemon → client)

```json
{"event": "started", "session": "s-17", "model": "parakeet-tdt-0.6b-v2-int8"}
{"event": "segment", "session": "s-17", "text": "Open the config file", "start": 0.42, "end": 2.10}
{"event": "partial", "session": "s-17", "text": "..."}            // reserved, not emitted in v1
{"event": "done", "session": "s-17",
 "text": "Open the config file.",       // after post-processing
 "raw_text": "Open the config file",    // before post-processing
 "segments": [{"text": "...", "start": 0.42, "end": 2.10}],
 "audio_ms": 2600,
 "reason": "stop",                      // stop | silence | max_duration | end_of_file
 "warnings": ["hotwords ignored: not enabled for this model"]}
{"event": "cancelled", "session": "s-17"}
{"event": "ack", "op": "stop", "session": "s-17", "id": 7}   // reply to stop/cancel
{"event": "status", "model": "parakeet-tdt-0.6b-v2-int8", "busy": false, "session": null, "version": "0.1.0",
  "capabilities": {"hotwords": false, "partials": false}}
{"event": "error", "code": "busy", "session": "s-17", "message": "session s-17 is active"}
```

Session ids are `s-<n>` with `n` a counter starting at 1. `status.session` is the active id or `null`. The `busy` error carries the active session id, so a toggle can't stop the wrong session. In stream mode, segments produced by the final flush are emitted as `segment` events before `done`. `audio_ms` is `floor(accepted_samples * 1000 / 16000)`; padding is never counted. A session with no speech ends with `done` and `text: ""`, `segments: []`.

Error codes: `busy`, `no_session`, `bad_request`, `bad_audio`, `audio_unavailable`, `unknown_postprocessor`. Post-processor failure is **not** an error, so speech is never lost: `done` still carries the last good text plus a warning.

`segment.text` is never revised. `done.text` is the authoritative result, because post-processing may rewrite it.

### Post-processor protocol

Each post-processor is an external command, spawned once per `done`.
- stdin, one JSON object: `{"text": "...", "context": "...", "hotwords": [...], "segments": [...]}`
- stdout, one JSON object: `{"text": "..."}`

Post-processors are chained: each one's `text` becomes the next one's input. If a post-processor exits non-zero, emits invalid JSON, or exceeds its timeout (default 10 s), it is skipped with a warning.

### CLI (`ursula`)

The CLI is a thin client over the same socket. It covers use cases 2 and 3 without writing any code.

| Command | Behavior |
|---|---|
| `ursula serve` | Run the daemon. |
| `ursula record [--stream] [--post NAME]... [--hotword W]... [--context TEXT] [--silence-stop MS]` | Start a mic session and block. Prints the final text to stdout (or one segment per line with `--stream`). Ctrl-C sends `stop`. |
| `ursula stop` / `ursula cancel` | Stop or cancel the active session. The `record` process prints the result. |
| `ursula toggle [record flags]` | `stop` if a session is active, else `record`. This is the single WM keybinding. |
| `ursula file PATH.wav` | Transcribe a file and print the text. |
| `ursula status` | Print the status JSON. |
| `ursula raw` | Pipe NDJSON from stdin to the socket and print events, for debugging. |

Global flags (before or after the subcommand): `--socket PATH` (default `$XDG_RUNTIME_DIR/ursula.sock`) and `--config PATH` (used by `serve`; default `$XDG_CONFIG_HOME/ursula/config.toml`). `--stream` conflicts with `--post`: streamed segments are final and never revised, so they can't also be post-processed. Errors go to stderr with exit status 1 (e.g. `stop` with no session prints the `no_session` error). The daemon holds `<socket>.lock` (never unlinked) so only one runs per socket; a stale socket file is replaced only if it really is a socket.

Example WM binding for use case 3: `ursula toggle | wtype -` (or `xdotool type --file -`). The first press records; the second press makes that same process print and pipe the text.

### NeoVim (reference client)

`lua/ursula/init.lua` lives in this repo and uses only `vim.uv.new_pipe()`: connect, write the `start` line, insert `segment` text as it arrives (or `done.text` if post-processors are used), send `stop` on the second keypress. It passes the current filetype and the nearby buffer lines as `context`. It has no external processes.

## Configuration

`$XDG_CONFIG_HOME/ursula/config.toml`. All fields are optional except the model paths, which the Nix modules generate.

```toml
[model]
kind = "parakeet-tdt"            # selects the Engine implementation and recognizer config
dir = "/nix/store/...-parakeet-tdt-0.6b-v2-int8"
name = "parakeet-tdt-0.6b-v2-int8"  # optional; default: dir basename minus "sherpa-onnx-nemo-"
threads = 4                      # default: min(4, available CPUs)
provider = "cpu"                 # "cuda" needs the ursula-cuda package

[vad]
model = "/nix/store/...-silero-vad/silero_vad.onnx"
threshold = 0.5
min_silence_ms = 500
min_speech_ms = 250
max_speech_ms = 20000            # hard split: no decoded segment exceeds this (engine slices PCM before decoding)

[session]
max_ms = 300000

[hotwords]
enabled = false                  # see Risks: upstream TDT hotword bug, needs bpe.vocab
score = 1.5
bpe_vocab = "/path/to/bpe.vocab"  # optional; default <model.dir>/bpe.vocab, must exist when enabled
words = ["NixOS", "Ursula"]      # merged with per-request hotwords

[postprocessors.cleanup]
command = ["/home/me/bin/haiku-cleanup"]
timeout_ms = 10000
```

## Nix

Flake outputs:
- `packages.default`: `ursula`, built against nixpkgs `sherpa-onnx` with `SHERPA_ONNX_LIB_DIR` set and the crate's `shared` feature. Without that, the crate's `build.rs` downloads binaries and fails in the sandbox.
- `packages.ursula-cuda`: the same, from a separate nixpkgs instance (`pkgsCuda`, `config = { allowUnfree = true; cudaSupport = true; }`), so both sherpa-onnx and onnxruntime are CUDA-enabled. It exists on `x86_64-linux` only, is never part of `checks`, and the default stays CPU. It is unfree and slow to build without a CUDA cache.
- `packages.<system>.{parakeet-tdt-0_6b-v2-int8, parakeet-tdt-0_6b-v3-int8, silero-vad}`: fixed-output derivations of the pinned sherpa-onnx release tarballs. The names are flat because flakes reject nested package sets.
- `apps.default`: `nix run` starts `ursula serve` with a generated default config.
- `devShells.default`: Rust toolchain, `sherpa-onnx`, `libpulseaudio`, `pkg-config`, the env vars, and model paths exported.
- `homeManagerModules.default`: `services.ursula = { enable; package; model; settings; autoStart; }`. It generates the config and a systemd user unit. `autoStart` (default `true`) controls whether the unit is wanted by `default.target`.
- `nixosModules.default`: the same options, but `autoStart` defaults to `false` (run `systemctl --user start ursula`), defining `systemd.user.services.ursula` for all users.
- `checks`: `build`, `clippy`, `test` (unit plus model-backed e2e, including `ursula file` on a bundled sample WAV), `mic-e2e`, `nixos-module-off`, `nixos-module-on`, `nixos-module-disabled`, `hm-module-on`, `hm-module-off`, `generated-config`.

## Milestones

M1–M4 and the `silence_stop_ms` and CUDA package items of M5 are implemented. Hotwords (blocked upstream) and the Nemotron engine are not.

1. **Core:** `ursula serve` plus `file` and `push` sources, VadOfflineEngine, the NDJSON protocol, the CLI, and the flake package and devShell. Latency and RSS are recorded under [Measurements](#measurements).
2. **Mic:** libpulse capture, stop/cancel/toggle, max duration, stall detection, platform validation on real PipeWire and WSL2 (WSLg) pending (see Risks).
3. **Service:** home-manager and NixOS modules, post-processors, config hotword list.
4. **Clients:** NeoVim reference plugin, example WM bindings.
5. **Nice-to-have:** `silence_stop_ms`, enabling hotwords once upstream is fixed, CUDA package, Nemotron engine.

## Risks and Open Questions

- **Hotwords on Parakeet TDT:** sherpa-onnx supports NeMo-transducer hotwords via `modified_beam_search`, but two things block it today. The published model packages lack the required `bpe.vocab`, which would have to be generated with upstream `scripts/nemo/generate_bpe_vocab.py` and shipped in the model derivation. There is also an open bug where TDT plus `modified_beam_search` yields empty or "Yeah." output on about 20% of utterances (fix in k2-fsa/sherpa-onnx#3657, unmerged as of 2026-10). Hotwords therefore ship disabled. The API accepts them anyway, and they always reach post-processors.
- **WSL2 microphone:** WSLg exposes the Windows mic as PulseAudio source `RDPSource`. There is no ALSA, and the Windows mic privacy setting must allow desktop apps. Recent open WSLg issues (#1482, #1508) report capture stalls. Mitigations are stall detection plus a clear error. A possible fallback is a small Windows-side capture tool that uses the `push` source. Not in v1.
- **Platform validation pending:** real PipeWire and WSLg microphones are untested by automation. Mic behaviour is tested only against a headless PulseAudio fed from a WAV file.
- **Opening the mic on demand** may clip the first ~50–100 ms of speech while the stream opens. Measure in M2. If it's a problem, add a short configurable keep-open linger after each session rather than recording all the time.
- **Memory:** there is no published figure for int8 Parakeet resident memory; it was estimated at ~0.7–1 GB. The measured figure is somewhat higher, see [Measurements](#measurements).
- **Simplifications** (known ceilings, upgrade path in parentheses):
  - Push input is 16 kHz s16le only (add a `rate` field and resample).
  - File input is 16 kHz WAV only (resample or decode other formats).
  - One session at a time (add per-session engines if ever needed).
  - The VAD is **not reset** after a soft-bound flush (`max_speech_ms`). A reset Silero state needs about 0.8 s to re-detect ongoing speech, and that speech was lost; flush already empties the buffer. VAD sample indices are therefore session-absolute.
