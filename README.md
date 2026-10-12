# Ursula

## What it is

Ursula is a local speech-to-text daemon for Linux (including WSL2). It keeps a Parakeet TDT model loaded, listens to your microphone only while a session is active, and hands back text. One binary, `ursula`, is both the daemon (`ursula serve`) and a small CLI client. Other programs talk to it with newline-delimited JSON over a Unix socket, so a keybinding, a NeoVim plugin, or an AI-agent script can all share one loaded model.

Speech is split by Silero VAD and each finished segment is decoded by the offline model. Everything runs on the CPU unless you opt into CUDA. See [DESIGN.md](DESIGN.md) for the protocol and the reasoning behind it.

## Disclaimer

 [DESIGN.md](DESIGN.md) was written entirely either by a human or with heavy human oversight. Everything else in this repository was written by agents based on that document with little human oversight. Use at your own risk.

## Install

### Nix

Run the daemon once, with a generated default config:

```sh
nix run github:guill/ursula            # or `nix run .` in a checkout
```

home-manager (`services.ursula.autoStart` defaults to `true`):

```nix
{
  inputs.ursula.url = "github:guill/ursula";
  # in your home-manager configuration:
  imports = [ ursula.homeManagerModules.default ];
  services.ursula = {
    enable = true;
    model = "parakeet-tdt-0.6b-v2-int8";   # or "parakeet-tdt-0.6b-v3-int8"
    # autoStart = true;
    # settings = { session.max_ms = 600000; };
  };
}
```

NixOS (installs a user unit for every user; `autoStart` defaults to `false`, so either set it or run `systemctl --user start ursula`):

```nix
{
  imports = [ ursula.nixosModules.default ];
  services.ursula = {
    enable = true;
    autoStart = true;
  };
}
```

Don't enable both modules for the same user. Each running daemon keeps about 1.1–1.2 GB (see DESIGN.md Measurements) of model resident.

### Without Nix

Build against an installed sherpa-onnx, then download the models:

```sh
SHERPA_ONNX_LIB_DIR=/path/to/sherpa-onnx/lib cargo build --release
scripts/download-models.sh            # v2 by default; `v3` for multilingual
```

The script verifies checksums and prints a TOML snippet to paste into `~/.config/ursula/config.toml`. libpulse is needed at build and run time.

## Config reference

The config lives at `$XDG_CONFIG_HOME/ursula/config.toml` (override with `--config`). Every key and default is documented under [Configuration](DESIGN.md#configuration) in DESIGN.md. Only `model.dir` and `vad.model` are required. `model.name` defaults to the model directory name without the `sherpa-onnx-nemo-` prefix.

## CLI usage

Global flags work before or after the subcommand:

- `--socket PATH`: daemon socket (default `$XDG_RUNTIME_DIR/ursula.sock`).
- `--config PATH`: config file for `serve`.

| Command | What it does |
|---|---|
| `ursula serve` | Run the daemon. Loads the model, binds the socket (mode 0600). Only one daemon per socket. |
| `ursula record [flags]` | Start a mic session and block. Prints the final text, or one segment per line with `--stream`. |
| `ursula stop` | Stop the active session. The `record` process prints the result. |
| `ursula cancel` | Discard the active session. |
| `ursula toggle [flags]` | `stop` if a session is active, else `record`. Takes the same flags as `record`. |
| `ursula file PATH.wav` | Transcribe a 16 kHz mono 16-bit WAV and print the text. |
| `ursula status` | Print the status JSON (`session` is `null` when idle). |
| `ursula raw` | Send NDJSON from stdin to the socket and print every event, for debugging. |

`record` and `toggle` flags:

- `--stream`: print each segment as it is decoded. Conflicts with `--post`.
- `--post NAME`: run a configured post-processor on the final text. Repeatable, applied in order.
- `--hotword W`: hotword to boost. Repeatable. Hotwords are off by default, see [DESIGN.md](DESIGN.md#risks-and-open-questions).
- `--context TEXT`: free text passed to post-processors.
- `--silence-stop MS`: finish after this much trailing silence (only after speech was heard).

Errors go to stderr and the exit status is 1 (flag misuse, like `--stream` with `--post`, exits 2). For example, `ursula stop` with nothing running prints the daemon's `no_session` error and exits 1. If the daemon isn't running, every client command fails with a connect error.

```sh
ursula status
ursula file ~/clip.wav
printf '{"op":"status"}\n' | ursula raw
```

## WM bindings

Text goes to stdout, so a binding can pipe it straight into a typing tool. The first press records. The second press stops that recording, and the first process prints the text.

sway / i3 (Wayland, needs `wtype`):

```
bindsym $mod+v exec ursula toggle | tr -d '\n' | wtype -
```

Hyprland:

```
bind = SUPER, V, exec, ursula toggle | tr -d '\n' | wtype -
```

X11:

```
bindsym $mod+v exec ursula toggle | tr -d '\n' | xdotool type --file -
```

`ursula toggle` prints the transcript plus a trailing newline, so the typing bindings strip it with `tr -d '\n'` to avoid pressing Enter. An agent usually wants the newline, so that example keeps it.

Feed an AI agent through a post-processor named `cleanup`:

```
bindsym $mod+Shift+v exec ursula toggle --post cleanup | my-agent
```

## NeoVim

`lua/ursula/init.lua` is a pure-Lua client (`vim.uv` only, no external processes). Add this repo to your runtimepath, then:

```lua
require("ursula").setup({
  postprocess = {},          -- e.g. { "cleanup" }
  hotwords = {},
  context_lines = 20,        -- nearby buffer lines sent as context
  -- socket = "/run/user/1000/ursula.sock",
})
vim.keymap.set({ "n", "i" }, "<F9>", require("ursula").toggle)
```

`setup` also defines `:Ursula toggle`, `:Ursula stop` and `:Ursula cancel`. Without post-processors each segment is inserted as it arrives. With them, the final text is inserted once at the end. While recording, a red `●` (highlight group `UrsulaRecording`) shows in the top-right corner. After inserting, the cursor moves to the end of the text, so recording again appends.

## WSL2 notes

These come from the design, not from testing on a real WSL2 machine. Treat them as unverified.

- WSLg exposes the Windows microphone as the PulseAudio source `RDPSource`. There is no ALSA.
- Windows must allow desktop apps to use the microphone (Settings, Privacy, Microphone).
- If the default source isn't right, point at it explicitly: `PULSE_SERVER` selects the server (WSLg uses `unix:/mnt/wslg/PulseServer`) and `PULSE_SOURCE=RDPSource` selects the source. Set them in the environment of `ursula serve`.
- If the mic stops delivering audio, the session ends with an `audio_unavailable` error. New mic sessions are refused until the stuck capture thread exits (when PulseAudio recovers). If it doesn't, restart ursula after a stall. At most one stuck thread can ever exist. File and push sessions keep working.

## CUDA

The default build is CPU-only. For GPU inference use `packages.x86_64-linux.ursula-cuda` (unfree, slow to build without a CUDA cache) and an NVIDIA driver, and set `provider = "cuda"` in `[model]`. Point the module `package` option at it:

```nix
services.ursula.package = ursula.packages.x86_64-linux.ursula-cuda;
services.ursula.settings.model.provider = "cuda";
```

The package is not in any binary cache, so expect a long build: about 4.4 h on a 16-thread desktop with `--cores 4 --max-jobs 1`. With the bundled int8 models it measured slower than the CPU build on an RTX 4090 (see [CPU vs GPU](DESIGN.md#cpu-vs-gpu-ursula-cuda) in DESIGN.md). The driver's `libcuda.so.1` must be visible to the daemon. On WSL2 that means `LD_LIBRARY_PATH=/usr/lib/wsl/lib`; elsewhere, your system's libcuda path.

## Post-processors

A post-processor is a command from the config. It receives one JSON object on stdin and must print one JSON object on stdout. It runs once per finished session, chained in order, and only on the final text. A non-zero exit, bad JSON, or a timeout skips it with a warning, so speech is never lost.

```toml
[postprocessors.cleanup]
command = ["/home/me/bin/cleanup"]
timeout_ms = 10000
```

```python
#!/usr/bin/env python3
import json, sys

req = json.load(sys.stdin)   # {"text", "context", "hotwords", "segments"}
text = req["text"].strip()
if text:
    text = text[0].upper() + text[1:]
json.dump({"text": text}, sys.stdout)
```

## Responsiveness under load

These are design choices, not hard real-time guarantees.

- `status` is answered out-of-band from a shared snapshot, so it stays prompt even while a session is busy. It isn't ordered against that connection's earlier requests.
- `cancel` should come from a separate connection (`ursula cancel` does this). It discards queued audio without decoding it. A full disconnect of the session owner also cancels.
- `stop` is ordered: it waits behind that connection's own queued audio, so pushed audio always reaches the engine first.
- Supervision (mic open timeout, stall detection) runs inside the controller loop at least every 100 ms.

## Development

```sh
nix develop                       # Rust toolchain, sherpa-onnx, libpulse, model paths
cargo test                        # unit tests; e2e tests skip without model env vars
URSULA_E2E_PULSE=1 cargo test --test e2e mic_ -- --test-threads=1   # fake-mic tests, need pulseaudio, pactl, ffmpeg
nix flake check
nvim --headless -u NONE --cmd "set rtp+=." -l tests/nvim/ursula_spec.lua <socket> <wav> [postprocessor]
```

The devShell exports `URSULA_E2E_MODEL_DIR` and `URSULA_E2E_VAD`, which the e2e tests need. If nix binaries abort because of a host `LD_LIBRARY_PATH`, run `nix develop --unset LD_LIBRARY_PATH`.
