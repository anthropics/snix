let
  passToSnixStoreDaemonAll = {
    useGrpcErrorPages = true;
    extraConfig = ''
      grpc_pass unix:/run/snix-store-daemon.sock;
      grpc_buffer_size 1m;
      client_max_body_size 0;
    '';
  };
  passToSnixStoreDaemonTrusted = {
    useGrpcErrorPages = true;
    extraConfig = ''
      # Trusted endpoints need mTLS
      if ($ssl_client_verify != SUCCESS) {
        return 401;
      }

      # We only allow certain DNs to talk to it
      if ($ssl_client_s_dn != "CN=build03.infra.snix.dev") {
        return 401;
      }

      ${passToSnixStoreDaemonAll.extraConfig}
    '';
  };

in
{ depot, pkgs, ... }:
{
  imports = [
    ./base.nix
  ];

  services.nginx.virtualHosts."cache.snix.dev" = {
    forceSSL = true;
    enableACME = true;
    extraConfig = ''
      ssl_client_certificate ${depot.ops.pki.ca_certificate};
      ssl_verify_client optional;
    '';

    locations."=/" = {
      tryFiles = "$uri $uri/index.html =404";
      root =
        let
          readme = builtins.toFile "README.md" ''
            # cache.snix.dev
            This is the binary cache for everything built by the Snix CI.

            Set it as a substituter if you want to reuse CI artifacts:

            ```nix
            nix.settings.trusted-public-keys = [
              "cache.snix.dev-1:miTqzIzmCbX/DyK2tLNXDROk77CbbvcRdWA4y2F8pno="
            ];
            nix.settings.substituters = [
              "https://cache.snix.dev"
            ];
            ```

            The cache is provided by `snix-store daemon` and `nar-bridge`.
            We also expose the `snix-[ca]store` gRPC interfaces on the same domain
            (read-only, no listing).

            Keep in mind there's no guarantees on paths being available, they might get
            GC'ed eventually.
          '';
        in
        pkgs.runCommand "index"
          {
            nativeBuildInputs = [ pkgs.markdown2html-converter ];
          }
          ''
            mkdir -p $out
            markdown2html-converter ${readme} -t cache.snix.dev -o $out/index.html
          '';
    };

    locations."/" = {
      proxyPass = "http://unix:/run/nar-bridge.sock:/";
      extraConfig = ''
        # Restrict allowed HTTP methods
        limit_except GET HEAD {
          # nar bridge allows to upload nars via PUT
          deny all;
        }

        # Propagate content-encoding to the backend
        proxy_set_header Accept-Encoding $http_accept_encoding;

        # Enable CORS from everywhere, same as c.n.o
        add_header Access-Control-Allow-Origin *;
      '';
    };

    locations."/grpc.reflection.v1alpha.ServerReflection" = passToSnixStoreDaemonAll;
    locations."/grpc.reflection.v1.ServerReflection" = passToSnixStoreDaemonAll;
    locations."/snix.castore.v1.BlobService/Stat" = passToSnixStoreDaemonAll;
    locations."/snix.castore.v1.BlobService/Read" = passToSnixStoreDaemonAll;
    locations."/snix.castore.v1.BlobService/Put" = passToSnixStoreDaemonTrusted;
    locations."/snix.castore.v1.DirectoryService/Get" = passToSnixStoreDaemonAll;
    locations."/snix.castore.v1.DirectoryService/Put" = passToSnixStoreDaemonTrusted;

    locations."/snix.store.v1.PathInfoService/Get" = passToSnixStoreDaemonAll;
    locations."/snix.store.v1.PathInfoService/Put" = passToSnixStoreDaemonTrusted;
    locations."/snix.store.v1.PathInfoService/CalculateNAR" = passToSnixStoreDaemonTrusted;
    locations."/snix.store.v1.PathInfoService/List" = passToSnixStoreDaemonTrusted;
  };
}
