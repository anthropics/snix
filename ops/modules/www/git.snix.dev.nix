{ ... }:

{
  imports = [
    ./base.nix
  ];

  config =
    let
      forgejoURI = "http://127.0.0.1:3000";
      iocaineURI = "http://127.0.0.1:42069";
    in
    {
      services.nginx.commonHttpConfig = ''
        map $request_method $forgejo_location {
          GET     ${iocaineURI};
          HEAD    ${iocaineURI};
          default ${forgejoURI};
        }
      '';
      services.nginx.virtualHosts.forgejo = {
        serverName = "git.snix.dev";
        enableACME = true;
        forceSSL = true;
        extraConfig = "recursive_error_pages on;";

        locations."/" = {
          recommendedProxySettings = true;
          proxyPass = "$forgejo_location";
          extraConfig = ''
            proxy_cache off;
            proxy_intercept_errors on;
            # proxy_pass $forgejo_location;
            error_page 421 = @fallback;
          '';
        };
        locations."@fallback" = {
          recommendedProxySettings = true;
          proxyPass = forgejoURI;
        };
      };
    };
}
