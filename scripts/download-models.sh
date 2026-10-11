#!/usr/bin/env bash
# Usage: download-models.sh [v2|v3] [target-dir]
# Downloads the sherpa-onnx Parakeet TDT tarball and silero VAD model, verifies
# sha256, extracts the tarball and prints a ready-to-paste TOML snippet.
# Behavior: a present file whose hash matches is reused. A present file with a bad
# hash is deleted and re-downloaded first. Only a checksum mismatch on a FRESHLY
# downloaded file deletes it and exits 1.
set -euo pipefail

version=${1:-v2}
case "$version" in
  v2) tar_name=sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2
      tar_sha=157c157bc51155e03e37d2466522a3a737dd9c72bb25f36eb18912964161e1ad ;;
  v3) tar_name=sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2
      tar_sha=5793d0fd397c5778d2cf2126994d58e9d56b1be7c04d13c7a15bb1b4eafb16bf ;;
  *) echo "usage: $0 [v2|v3] [target-dir]" >&2; exit 2 ;;
esac

dir=${2:-${XDG_DATA_HOME:-$HOME/.local/share}/ursula/models}
mkdir -p "$dir"
dir=$(cd "$dir" && pwd)

base=https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models
silero_sha=9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6

verify() { printf '%s  %s\n' "$1" "$2" | sha256sum -c --status; }

# fetch <url> <sha256> <dest> -> prints "cached" or "fresh"; exits 1 on fresh mismatch.
fetch() {
  local url=$1 sha=$2 dest=$3
  if [[ -f $dest ]] && verify "$sha" "$dest"; then echo cached; return; fi
  rm -f "$dest"
  curl -fL --retry 3 -o "$dest.part" "$url"
  mv "$dest.part" "$dest"
  if ! verify "$sha" "$dest"; then
    rm -f "$dest"
    echo "checksum mismatch (fresh download, deleted): $dest" >&2
    exit 1
  fi
  echo fresh
}

model_name=${tar_name%.tar.bz2}
model_dir=$dir/$model_name
silero=$dir/silero_vad.onnx

tar_state=$(fetch "$base/$tar_name" "$tar_sha" "$dir/$tar_name")
if [[ $tar_state == fresh || ! -f $model_dir/encoder.int8.onnx ]]; then
  tar -xjf "$dir/$tar_name" -C "$dir"
fi
silero_state=$(fetch "$base/silero_vad.onnx" "$silero_sha" "$silero")

printf 'tarball: %s, silero: %s\n' "$tar_state" "$silero_state" >&2
printf '[model]\ndir = "%s"\n[vad]\nmodel = "%s"\n' "$model_dir" "$silero"
