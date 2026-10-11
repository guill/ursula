# Shared by hm-module.nix, nixos-module.nix and the flake's `lib.mkConfig`.
self:
{ lib, pkgs, ... }:
let
  flakePkgs = self.packages.${pkgs.stdenv.hostPlatform.system};
  toml = pkgs.formats.toml { };
  models = {
    "parakeet-tdt-0.6b-v2-int8" = flakePkgs.parakeet-tdt-0_6b-v2-int8;
    "parakeet-tdt-0.6b-v3-int8" = flakePkgs.parakeet-tdt-0_6b-v3-int8;
  };
in
{
  options = {
    enable = lib.mkEnableOption "the ursula speech-to-text daemon (systemd user service)";
    package = lib.mkOption {
      type = lib.types.package;
      default = flakePkgs.default;
      defaultText = lib.literalExpression "ursula.packages.\${system}.default";
      description = "The ursula package.";
    };
    model = lib.mkOption {
      type = lib.types.enum (builtins.attrNames models);
      default = "parakeet-tdt-0.6b-v2-int8";
      description = "Speech model; its files come from the flake's pinned model packages.";
    };
    settings = lib.mkOption {
      inherit (toml) type;
      default = { };
      description = "Extra config.toml settings, merged over the generated model/VAD paths.";
    };
  };

  # The explicit name keeps the store hash out of `status`/`started` (it would be derived from dir's basename).
  mkConfig =
    cfg:
    toml.generate "ursula-config.toml" (
      lib.recursiveUpdate {
        model = {
          name = cfg.model;
          dir = "${models.${cfg.model}}";
        };
        vad.model = "${flakePkgs.silero-vad}/silero_vad.onnx";
      } cfg.settings
    );
}
