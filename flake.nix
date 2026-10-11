{
  description = "ursula: local speech-to-text daemon (sherpa-onnx)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    # Used only by the hm-module checks.
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      home-manager,
      ...
    }:
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
          # Plain import: callPackage would add non-derivation override attrs to `packages`.
          models = import ./nix/models.nix { inherit (pkgs) fetchurl stdenvNoCC; };
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
      # Evaluate the modules and grep the results (one runCommand per check).
      moduleChecks =
        system:
        let
          inherit (perSystem system) pkgs;
          check =
            name: asserts:
            pkgs.runCommand name { } ''
              ${asserts}
              touch $out
            '';
          json = v: nixpkgs.lib.escapeShellArg (builtins.toJSON v);
          nixosUnits =
            ursulaCfg:
            (nixpkgs.lib.nixosSystem {
              inherit system;
              modules = [
                self.nixosModules.default
                {
                  boot.isContainer = true;
                  system.stateVersion = "26.05";
                }
                { services.ursula = ursulaCfg; }
              ];
            }).config.systemd.user.services;
          hmUnit =
            ursulaCfg:
            let
              home = home-manager.lib.homeManagerConfiguration {
                inherit pkgs;
                modules = [
                  self.homeManagerModules.default
                  {
                    home.username = "t";
                    home.homeDirectory = "/home/t";
                    home.stateVersion = "26.05";
                    services.ursula = ursulaCfg;
                  }
                ];
              };
            in
            "${home.activationPackage}/home-files/.config/systemd/user/ursula.service";
          nixosOff = (nixosUnits { enable = true; }).ursula;
          nixosOn =
            (nixosUnits {
              enable = true;
              autoStart = true;
            }).ursula;
          hmOn = hmUnit { enable = true; };
          hmOff = hmUnit {
            enable = true;
            autoStart = false;
          };
          generated = self.lib.mkConfig pkgs {
            model = "parakeet-tdt-0.6b-v2-int8";
            settings = { };
          };
        in
        {
          nixos-module-off = check "nixos-module-off" ''
            echo ${json nixosOff.wantedBy} | grep -qxF '[]'
            echo ${json nixosOff.serviceConfig.ExecStart} | grep -qF 'ursula serve --config'
          '';
          nixos-module-on = check "nixos-module-on" ''
            echo ${json nixosOn.wantedBy} | grep -qxF '["default.target"]'
          '';
          nixos-module-disabled = check "nixos-module-disabled" ''
            echo ${json (!(nixosUnits { enable = false; }) ? ursula)} | grep -qx true
          '';
          hm-module-on = check "hm-module-on" ''
            grep -q '^ExecStart=' ${hmOn}
            grep -qx 'WantedBy=default.target' ${hmOn}
          '';
          # `! grep` would be ignored by `set -e`; also require the unit to exist.
          hm-module-off = check "hm-module-off" ''
            grep -q '^ExecStart=' ${hmOff}
            if grep -q '^WantedBy=' ${hmOff}; then exit 1; fi
          '';
          generated-config = check "generated-config" ''
            grep -qF 'name = "parakeet-tdt-0.6b-v2-int8"' ${generated}
          '';
        };
    in
    {
      homeManagerModules.default = import ./nix/hm-module.nix self;
      nixosModules.default = import ./nix/nixos-module.nix self;
      lib.mkConfig =
        pkgs:
        { model, settings }:
        (import ./nix/module-common.nix self {
          inherit pkgs;
          inherit (pkgs) lib;
        }).mkConfig
          { inherit model settings; };

      packages = forSystems (
        system:
        let
          s = perSystem system;
        in
        s.models
        // {
          default = s.ursula;
        }
        # Opt-in CUDA build; cudaSupport is scoped to pkgsCuda, never global.
        // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") {
          ursula-cuda =
            let
              pkgsCuda = import nixpkgs {
                inherit system;
                config = {
                  allowUnfree = true;
                  cudaSupport = true;
                };
              };
            in
            pkgsCuda.callPackage ./nix/package.nix { craneLib = crane.mkLib pkgsCuda; };
        }
      );

      # `nix run . -- --socket X`: global flags go before `serve` (clap accepts that).
      apps = forSystems (
        system:
        let
          inherit (perSystem system) pkgs ursula;
          defaultConfig = self.lib.mkConfig pkgs {
            model = "parakeet-tdt-0.6b-v2-int8";
            settings = { };
          };
          launcher = pkgs.writeShellApplication {
            name = "ursula-serve";
            text = ''exec ${ursula}/bin/ursula "$@" serve --config ${defaultConfig}'';
          };
        in
        {
          default = {
            type = "app";
            program = "${launcher}/bin/ursula-serve";
            meta.description = "Run ursula serve with a generated default (v2 model) config";
          };
        }
      );

      checks = forSystems (
        system:
        let
          inherit (perSystem system)
            pkgs
            craneLib
            models
            ursula
            ;
          inherit (ursula.passthru) common cargoArtifacts;
        in
        {
          build = ursula;
          clippy = craneLib.cargoClippy (
            common
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- -D warnings";
            }
          );
          # Unit tests plus the model-backed e2e tests (stdenv already provides sh/coreutils).
          test = craneLib.cargoTest (
            common
            // {
              inherit cargoArtifacts;
              URSULA_E2E_MODEL_DIR = "${models.parakeet-tdt-0_6b-v2-int8}";
              URSULA_E2E_VAD = "${models.silero-vad}/silero_vad.onnx";
              # Show `skipped:` lines (and measured latencies) in the build log.
              cargoTestExtraArgs = "-- --nocapture";
            }
          );
          # simplification: sandbox may lack pulseaudio; devShell run is authoritative
          mic-e2e = craneLib.cargoTest (
            common
            // {
              inherit cargoArtifacts;
              URSULA_E2E_MODEL_DIR = "${models.parakeet-tdt-0_6b-v2-int8}";
              URSULA_E2E_VAD = "${models.silero-vad}/silero_vad.onnx";
              URSULA_E2E_PULSE = "1";
              nativeBuildInputs = common.nativeBuildInputs ++ [
                pkgs.pulseaudio
                pkgs.ffmpeg
              ];
              cargoTestExtraArgs = "--test e2e mic_ -- --nocapture --test-threads=1";
            }
          );
        }
        // moduleChecks system
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
