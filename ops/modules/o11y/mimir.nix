{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.depot.prometheus;
  inherit (lib) mkEnableOption mkIf;

  mimirPort = config.services.mimir.configuration.server.http_listen_port;

  alerts =
    pkgs.runCommand "mimir-alerts-checked"
      {
        src = ./alerts;
        nativeBuildInputs = with pkgs; [ prometheus.cli ];
      }
      ''
        promtool check rules $src/*
        mkdir $out
        cp -R $src $out/anonymous/
      '';
in
{
  options.services.depot.prometheus.enable = mkEnableOption "Prometheus scraper";

  config = mkIf cfg.enable {
    services.mimir = {
      enable = true;
      extraFlags = [ "--config.expand-env=true" ];
      configuration = {
        target = "all,alertmanager";

        multitenancy_enabled = false;

        common.storage = {
          backend = "s3";
          s3 = {
            endpoint = "fsn1.your-objectstorage.com";
            bucket_name = "snix-mimir";
            secret_access_key = "\${S3_KEY}"; # This is a secret injected via an environment variable
            access_key_id = "\${S3_KEY_ID}";
          };
        };

        # TODO: Such a ugly hack.
        distributor.ring.instance_interface_names = [
          "enp1s0"
          "lo"
        ];
        ingester.ring.instance_interface_names = [
          "enp1s0"
          "lo"
        ];
        frontend.instance_interface_names = [
          "enp1s0"
          "lo"
        ];
        querier.ring.instance_interface_names = [
          "enp1s0"
          "lo"
        ];
        query_scheduler.ring.instance_interface_names = [
          "enp1s0"
          "lo"
        ];
        ruler.ring.instance_interface_names = [
          "enp1s0"
          "lo"
        ];
        compactor.sharding_ring.instance_interface_names = [
          "enp1s0"
          "lo"
        ];
        store_gateway.sharding_ring.instance_interface_names = [
          "enp1s0"
          "lo"
        ];

        memberlist = {
          advertise_addr = "127.0.0.1";
          cluster_label = "snix";
        };

        server = {
          http_listen_port = 9009;
          grpc_server_max_recv_msg_size = 104857600;
          grpc_server_max_send_msg_size = 104857600;
          grpc_server_max_concurrent_streams = 1000;
        };

        ingester.ring.replication_factor = 1;

        distributor.instance_limits.max_ingestion_rate = 0; # unlimited
        limits = {
          ingestion_rate = 1000000; # can't set to unlimited :(
          out_of_order_time_window = "12h";
          max_global_series_per_user = 0; # unlimited
        };

        blocks_storage.backend = "s3";
        ruler_storage = {
          backend = "local";
          local.directory = alerts;
        };

        alertmanager = {
          sharding_ring = {
            replication_factor = 1;
            # TODO: hack
            instance_interface_names = [ "enp1s0" ];
          };
          fallback_config_file = pkgs.writers.writeYAML "alertmanager.yaml" {
            route = {
              group_by = [ "alertname" ];
              receiver = "irc";
            };
            receivers = [
              {
                name = "irc";
                webhook_configs = [
                  {
                    # Mimir can't expand environment variables in external config files,
                    # so work around it.
                    url_file = "/run/credentials/mimir.service/webhook-url";
                  }
                ];
              }
            ];
          };
        };
        alertmanager_storage.backend = "filesystem";

        ruler.alertmanager_url = "http://localhost:${toString mimirPort}/alertmanager";
      };
    };

    systemd.services.mimir = {
      # Mimir tries to determine its own IP address for gossip purposes,
      # even when it's the only instance, and fails if it can't find one.
      # Avoid that by ensuring it starts after the network is set up.
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      serviceConfig = {
        EnvironmentFile = [ config.age.secrets.mimir-environment.path ];
        LoadCredential = [ "webhook-url:${config.age.secrets.mimir-webhook-url.path}" ];
      };
    };

    infra.monitoring.alloy.exporters.mimir.port = 9009;
  };
}
