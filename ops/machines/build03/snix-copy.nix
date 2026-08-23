{
  depot,
  config,
  pkgs,
  ...
}:

let
  tls_client_cert_path = depot.ops.pki.host_certificates."build03.infra.snix.dev";
  tls_client_key_path = "/run/credentials/snix-copy.service/mtls-private-key.pem";
  binary_cache_key_path = "/run/credentials/snix-copy.service/binary-cache-key";
  store_url = "grpc+https://cache.snix.dev?tls-client-cert-path=${tls_client_cert_path}&tls-client-key-path=${tls_client_key_path}";

  storeCompositionFile = (pkgs.formats.toml { }).generate "store-composition.toml" {
    blobservices = {
      root = {
        type = "grpc";
        url = store_url;
      };
    };

    directoryservices = {
      root = {
        type = "grpc";
        url = store_url;
      };
    };

    pathinfoservices = {
      root = {
        type = "keyfile-signing";
        keyfile = binary_cache_key_path;
        # sign, then upload via gRPC.
        inner = "&grpc";
      };
      grpc = {
        type = "grpc";
        url = store_url;
      };
    };

  };
in
{
  age.secrets.binary-cache-key.file = depot.ops.secrets."binary-cache-key.age";

  systemd.services.snix-copy = {
    environment.OTEL_SERVICE_NAME = "snix.store.copy";
    environment.TRACER = "otlp";
    serviceConfig = {
      ExecStart =
        "${depot.snix.cli.store.with-features-xp-store-composition-cli}/bin/snix-store copy"
        + " --experimental-store-composition ${storeCompositionFile}"
        + " --jsonl /run/watch-store.sock";
      Type = "simple";
      User = "snix-copy";
      DynamicUser = true;
      ProtectHome = true;
      ProtectSystem = true;
      MemoryDenyWriteExecute = true;
      ProtectControlGroups = true;
      ProtectKernelModules = true;
      ProtectKernelTunables = true;
      Restart = "on-failure";
      RestartSec = 5;
      RestrictNamespaces = true;
      RestrictRealtime = true;
      SystemCallArchitectures = "native";
      SystemCallFilter = [
        "@system-service"
        "~@privileged"
      ];
      LoadCredential = [
        "mtls-private-key.pem:${config.age.secrets.mtls-private-key.path}"
        "binary-cache-key:${config.age.secrets.binary-cache-key.path}"
      ];
    };
    wantedBy = [ "multi-user.target" ];
  };
}
