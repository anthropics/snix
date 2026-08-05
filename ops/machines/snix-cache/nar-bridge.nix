{
  config,
  depot,
  ...
}:
let
  mod = name: depot.path.origSrc + ("/ops/modules/" + name);
in

{
  imports = [
    (mod "nar-bridge.nix")
    (mod "snix-store-daemon.nix")
  ];

  # Explicitly configure uid/gid to ensure they match the uid/gid of the
  # blobservice data in /tank/nar-bridge, as chown'ing all data there is no fun.
  users.users.snix-store-daemon.uid = 998;
  users.groups.snix-store-daemon.gid = 998;

  services.snix-store-daemon = {
    enable = true;
    package = depot.snix.cli.store.with-features-xp-store-composition-cli-otlp;

    settings = {
      blobservices.root = {
        type = "objectstore";
        object_store_url = "file:///tank/snix-castore/blobs.object_store";
        object_store_options = { };
      };

      directoryservices.root = {
        type = "redb";
        path = "/var/lib/snix-store/directories.redb";
      };

      pathinfoservices = {
        root = {
          type = "cache";
          near = "&redb";
          far = "&cache-nixos-org";
        };

        redb = {
          type = "redb";
          path = "/var/lib/snix-store/pathinfo.redb";
        };

        "cache-nixos-org" = {
          type = "nix";
          base_url = "https://cache.nixos.org";
          blob_service = "&root";
          directory_service = "&root";
          trusted_public_keys = [
            "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
          ];
        };
      };
    };
  };
  systemd.services.snix-store-daemon = {
    # #229
    # environment.TRACER = "otlp";
    # Ensure /tank is mounted, which is where we the blobservice reads from.
    unitConfig.RequiresMountsFor = "/tank";

    # twice the normal allowed limit, same as nix-daemon
    serviceConfig.LimitNOFILE = "1048576";
  };

  services.nar-bridge = {
    enable = true;
    package = depot.snix.cli.nar-bridge.with-features-xp-store-composition-cli-otlp;

    settings = {
      blobservices.root = {
        type = "grpc";
        url = "grpc+unix:/run/snix-store-daemon.sock";
      };

      directoryservices.root = {
        type = "grpc";
        url = "grpc+unix:/run/snix-store-daemon.sock";
      };

      pathinfoservices.root = {
        type = "grpc";
        url = "grpc+unix:/run/snix-store-daemon.sock";
      };
    };
  };

  # #229
  # systemd.services.nar-bridge.environment.TRACER = "otlp";

  systemd.tmpfiles.rules = [
    # Cache responses on NVME
    "d /var/cache/nginx 0755 ${config.services.nginx.user} ${config.services.nginx.group} -"

    # Put the blobs on the big disk
    "v /tank/snix-castore                    0755 snix-store-daemon snix-store-daemon -"
    "v /tank/snix-castore/blobs.object_store 0755 snix-store-daemon snix-store-daemon -"
  ];
}
