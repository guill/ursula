{ fetchurl, stdenvNoCC }:
let
  base = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models";
  tarball =
    name: hash:
    stdenvNoCC.mkDerivation {
      inherit name;
      src = fetchurl {
        url = "${base}/sherpa-onnx-nemo-${name}.tar.bz2";
        inherit hash;
      };
      # The tarball holds one top-level dir; unpackPhase cds into it.
      dontConfigure = true;
      dontBuild = true;
      dontFixup = true;
      installPhase = ''
        mkdir -p $out
        cp -r . $out/
      '';
    };
in
{
  parakeet-tdt-0_6b-v2-int8 = tarball "parakeet-tdt-0.6b-v2-int8" "sha256-FXwVe8URVeA+N9JGZSKjpzfdnHK7JfNusYkSlkFh4a0=";
  parakeet-tdt-0_6b-v3-int8 = tarball "parakeet-tdt-0.6b-v3-int8" "sha256-V5PQ/Tl8V3jSzyEmmU1Y6dVrG+fATRPHoVuxtOr7Fr8=";
  silero-vad = stdenvNoCC.mkDerivation {
    name = "silero-vad";
    src = fetchurl {
      url = "${base}/silero_vad.onnx";
      hash = "sha256-niRJ4Qh0ltjUyrqQfyPgvT942R+lUkebucI6wJy7H9Y=";
    };
    dontUnpack = true;
    installPhase = ''
      mkdir -p $out
      cp $src $out/silero_vad.onnx
    '';
  };
}
