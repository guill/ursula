{
  craneLib,
  lib,
  sherpa-onnx,
  onnxruntime,
  libpulseaudio,
  pkg-config,
  symlinkJoin,
}:
let
  src = craneLib.cleanCargoSource ../.;
  # sherpa-onnx-sys wants libsherpa-onnx-c-api.so and libonnxruntime.so in one dir.
  sherpaLibs = symlinkJoin {
    name = "sherpa-onnx-libs";
    paths = [
      sherpa-onnx
      onnxruntime
    ];
  };
  common = {
    inherit src;
    strictDeps = true;
    SHERPA_ONNX_LIB_DIR = "${sherpaLibs}/lib";
    cargoExtraArgs = "--locked";
    buildInputs = [
      sherpa-onnx
      onnxruntime
      libpulseaudio
    ];
    nativeBuildInputs = [ pkg-config ];
  };
  cargoArtifacts = craneLib.buildDepsOnly common;
in
craneLib.buildPackage (
  common
  // {
    inherit cargoArtifacts;
    doCheck = false;
    passthru = { inherit cargoArtifacts common; };
    meta.mainProgram = "ursula";
  }
)
