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
  parakeet-tdt-0_6b-v2-int8 = tarball "parakeet-tdt-0.6b-v2-int8" "sha256-b8RCa1SqVJoAGbinW43qWRZOcXkKN6xWoEWtV/kV9xQ=";
  parakeet-tdt-0_6b-v3-int8 = tarball "parakeet-tdt-0.6b-v3-int8" "sha256-FJXpG1nX/FPdypHzQxdY7Jn4+iU/2w4ehRvg9ysUsPk=";
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
