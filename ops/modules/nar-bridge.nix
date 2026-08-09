{
  config,
  lib,
  utils,
  pkgs,
  depot,
  ...
}:
let
  cfg = config.services.nar-bridge;

  storeCompositionFormat = pkgs.formats.toml { };

  storeCompositionFile = storeCompositionFormat.generate "store-composition.toml" cfg.settings;

  args = [
    "--listen-address"
    "sd-listen"
    "--experimental-store-composition"
    storeCompositionFile
  ];
in
{
  options = {
    services.nar-bridge = {
      enable = lib.mkEnableOption "nar-bridge service";

      package =
        lib.mkPackageOption depot "snix.cli.nar-bridge.with-features-xp-store-composition-cli"
          { };

      settings = lib.mkOption {
        type = storeCompositionFormat.type;
        default = { };
      };
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.sockets.nar-bridge = {
      description = "nar-bridge socket";
      wantedBy = [ "sockets.target" ];

      socketConfig = {
        ListenStream = "/run/nar-bridge.sock";
        SocketMode = "0666";
        SocketUser = "root";
      };
    };

    systemd.services.nar-bridge = {
      description = "NAR Bridge";
      requires = [ "nar-bridge.socket" ];
      after = [ "nar-bridge.socket" ];
      wantedBy = [ "multi-user.target" ];
      environment.OTEL_SERVICE_NAME = "snix.nar-bridge";
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/snix-nar-bridge ${utils.escapeSystemdExecArgs args}";
        LimitNOFILE = lib.mkDefault 65535;

        Restart = "always";
        RestartSec = "10";

        User = "nar-bridge";
        Group = "nar-bridge";
        DynamicUser = true;
        StateDirectory = "nar-bridge";
      };
    };
  };
}
