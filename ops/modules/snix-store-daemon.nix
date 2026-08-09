{
  config,
  lib,
  utils,
  pkgs,
  depot,
  ...
}:
let
  cfg = config.services.snix-store-daemon;

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
    services.snix-store-daemon = {
      enable = lib.mkEnableOption "snix-store-daemon service";

      package = lib.mkPackageOption depot "snix.cli.store.with-features-xp-store-composition-cli" { };

      settings = lib.mkOption {
        type = storeCompositionFormat.type;
        default = { };
      };
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.snix-store-daemon = {
      isSystemUser = true;
      group = "snix-store-daemon";
    };

    users.groups.snix-store-daemon = { };

    systemd.sockets.snix-store-daemon = {
      description = "snix-store-daemon socket";
      wantedBy = [ "sockets.target" ];

      socketConfig = {
        ListenStream = "/run/snix-store-daemon.sock";
        SocketMode = "0666";
        SocketUser = "root";
      };
    };

    systemd.services.snix-store-daemon = {
      description = "Snix Store Daemon";
      requires = [ "snix-store-daemon.socket" ];
      after = [ "snix-store-daemon.socket" ];
      wantedBy = [ "multi-user.target" ];
      environment.OTEL_SERVICE_NAME = "snix.snix-store";
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/snix-store daemon ${utils.escapeSystemdExecArgs (args)}";
        LimitNOFILE = lib.mkDefault 65535;

        Restart = "always";
        RestartSec = "10";

        User = "snix-store-daemon";
        Group = "snix-store-daemon";
        StateDirectory = "snix-store";
      };
    };
  };
}
