{
  description = "ursula: local speech-to-text daemon (sherpa-onnx)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    # Used only by checks (todo 15).
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, crane, ... }:
    let
      forSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
      perSystem =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          craneLib = crane.mkLib pkgs;
          models = pkgs.callPackage ./nix/models.nix { };
          ursula = pkgs.callPackage ./nix/package.nix { inherit craneLib; };
        in
        {
          inherit
            pkgs
            craneLib
            models
            ursula
            ;
        };
    in
    {
      packages = forSystems (
        system:
        let
          s = perSystem system;
        in
        s.models // { default = s.ursula; }
      );

      devShells = forSystems (
        system:
        let
          inherit (perSystem system)
            pkgs
            craneLib
            models
            ursula
            ;
        in
        {
          default = craneLib.devShell {
            inputsFrom = [ ursula ];
            packages = [
              pkgs.rust-analyzer
              pkgs.pulseaudio
              pkgs.ffmpeg
              pkgs.neovim
            ];
            inherit (ursula.passthru.common) SHERPA_ONNX_LIB_DIR;
            URSULA_E2E_MODEL_DIR = "${models.parakeet-tdt-0_6b-v2-int8}";
            URSULA_E2E_VAD = "${models.silero-vad}/silero_vad.onnx";
            shellHook = ''
              # Host LD_LIBRARY_PATH may point at an older glibc; nixpkgs binaries then abort.
              unset LD_LIBRARY_PATH
              export PATH="$PWD/target/debug:$PATH"
            '';
          };
        }
      );
    };
}
