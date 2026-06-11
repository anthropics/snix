let
  passToSnixStoreDaemonAll = ''
    grpc_pass unix:/run/snix-store-daemon.sock;
    grpc_buffer_size 1m;

    client_max_body_size 0;

    error_page 400 = @grpc_internal;
    error_page 401 = @grpc_unauthenticated;
    error_page 403 = @grpc_permission_denied;
    error_page 404 = @grpc_unimplemented;
    error_page 429 = @grpc_unavailable;
    error_page 502 = @grpc_unavailable;
    error_page 503 = @grpc_unavailable;
    error_page 504 = @grpc_unavailable;
    # NGINX-to-gRPC status code mappings
    # Ref: https://github.com/grpc/grpc/blob/master/doc/statuscodes.md
    #
    error_page 405 = @grpc_internal; # Method not allowed
    error_page 408 = @grpc_deadline_exceeded; # Request timeout
    error_page 413 = @grpc_resource_exhausted; # Payload too large
    error_page 414 = @grpc_resource_exhausted; # Request URI too large
    error_page 415 = @grpc_internal; # Unsupported media type;
    error_page 426 = @grpc_internal; # HTTP request was sent to HTTPS port
    error_page 495 = @grpc_unauthenticated; # Client certificate authentication error
    error_page 496 = @grpc_unauthenticated; # Client certificate not presented
    error_page 497 = @grpc_internal; # HTTP request was sent to mutual TLS port
    error_page 500 = @grpc_internal; # Server error
    error_page 501 = @grpc_internal; # Not implemented
  '';
  passToSnixStoreDaemonTrusted = ''
    # Trusted endpoints need mTLS
    if ($ssl_client_verify != SUCCESS) {
      return 401;
    }

    # We only allow certain DNs to talk to it
    if ($ssl_client_s_dn != "CN=build03.infra.snix.dev") {
      return 401;
    }

    ${passToSnixStoreDaemonAll}
  '';

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

      # gRPC error responses
      # Ref: https://github.com/grpc/grpc-go/blob/master/codes/codes.go
      #
      location @grpc_deadline_exceeded {
          add_header grpc-status 4;
          add_header grpc-message 'deadline exceeded';
          default_type application/grpc;
          return 204;
      }
      location @grpc_permission_denied {
          add_header grpc-status 7;
          add_header grpc-message 'permission denied';
          default_type application/grpc;
          return 204;
      }
      location @grpc_resource_exhausted {
          add_header grpc-status 8;
          add_header grpc-message 'resource exhausted';
          default_type application/grpc;
          return 204;
      }
      location @grpc_unimplemented {
          add_header grpc-status 12;
          add_header grpc-message unimplemented;
          default_type application/grpc;
          return 204;
      }
      location @grpc_internal {
          add_header grpc-status 13;
          add_header grpc-message 'internal error';
          default_type application/grpc;
          return 204;
      }
      location @grpc_unavailable {
          add_header grpc-status 14;
          add_header grpc-message unavailable;
          default_type application/grpc;
          return 204;
      }
      location @grpc_unauthenticated {
          add_header grpc-status 16;
          add_header grpc-message unauthenticated;
          default_type application/grpc;
          return 200;
      }
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

    locations."/grpc.reflection.v1alpha.ServerReflection".extraConfig = passToSnixStoreDaemonAll;
    locations."/grpc.reflection.v1.ServerReflection".extraConfig = passToSnixStoreDaemonAll;
    locations."/snix.castore.v1.BlobService/Stat".extraConfig = passToSnixStoreDaemonAll;
    locations."/snix.castore.v1.BlobService/Read".extraConfig = passToSnixStoreDaemonAll;
    locations."/snix.castore.v1.BlobService/Put".extraConfig = passToSnixStoreDaemonTrusted;
    locations."/snix.castore.v1.DirectoryService/Get".extraConfig = passToSnixStoreDaemonAll;
    locations."/snix.castore.v1.DirectoryService/Put".extraConfig = passToSnixStoreDaemonTrusted;

    locations."/snix.store.v1.PathInfoService/Get".extraConfig = passToSnixStoreDaemonAll;
    locations."/snix.store.v1.PathInfoService/Put".extraConfig = passToSnixStoreDaemonTrusted;
    locations."/snix.store.v1.PathInfoService/CalculateNAR".extraConfig = passToSnixStoreDaemonTrusted;
    locations."/snix.store.v1.PathInfoService/List".extraConfig = passToSnixStoreDaemonTrusted;
  };
}
