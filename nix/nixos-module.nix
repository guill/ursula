self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  common = import ./module-common.nix self { inherit lib pkgs; };
  cfg = config.services.ursula;
in
{
  options.services.ursula = common.options // {
    enable = common.options.enable // {
      description = "Whether to enable ursula. Enabling installs a user unit for every user; each running instance keeps ~1 GB of model resident; do not also enable the home-manager module for the same user.";
    };
    autoStart = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Start the daemon with every user session (default.target); otherwise run `systemctl --user start ursula`.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.ursula = {
      description = "Ursula voice transcription";
      after = [ "pipewire-pulse.service" ];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/ursula serve --config ${common.mkConfig cfg}";
        Restart = "on-failure";
      };
      wantedBy = lib.optional cfg.autoStart "default.target";
    };
  };
}
