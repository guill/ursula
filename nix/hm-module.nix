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
    autoStart = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Start the daemon with the user session (default.target).";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.ursula = {
      Unit = {
        Description = "Ursula voice transcription";
        After = [ "pipewire-pulse.service" ];
      };
      Service = {
        ExecStart = "${cfg.package}/bin/ursula serve --config ${common.mkConfig cfg}";
        Restart = "on-failure";
      };
      Install.WantedBy = lib.optional cfg.autoStart "default.target";
    };
  };
}
