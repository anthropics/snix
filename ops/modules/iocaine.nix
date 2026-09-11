{ lib, ... }:

let
  genAgentRange =
    agent: from: to: sep: trailer:
    map (n: "${agent}${sep}${toString n}${trailer}") (lib.range from to);

in
{
  services.iocaine = {
    enable = true;
    settings.initial-seed-file = "/run/current-system/boot.json";
    settings.server.default = {
      bind = "127.0.0.1:42069";
      mode = "http";
      use.handler-from = "default";
      use.metrics = "metrics";
    };
    settings.server.metrics = {
      bind = "127.0.0.1:42042";
      mode = "prometheus";
      persist-path = "qmk-metrics.json";
      persist-interval = "1h";
    };
    settings.handler.default = {
      config = {
        unwanted-visitors =
          # broad version ranges
          (genAgentRange "Android" 2 12 " " ".")
          ++ (genAgentRange "Chrome" 1 148 "/" ".")
          ++ (genAgentRange "CriOS" 1 142 "/" ".")
          ++ (genAgentRange "Firefox" 1 139 "/" ".")
          ++ (genAgentRange "Firefox" 141 150 "/" ".")
          ++ (genAgentRange "FxiOS" 1 150 "/" ".")
          ++ (genAgentRange "iPhone OS" 1 14 " " "_")
          ++ (genAgentRange "Windows NT" 4 7 " " "")
          ++ (genAgentRange "Mac OS X 10." 5 14 "" "")
          ++ (genAgentRange "Mac OS X 10_" 5 14 "" "")
          ++ (genAgentRange "Mac OS X" 11 14 " " "")
          ++ [
            # manually crafted patterns
            "iPod;"
            "Presto/"
            "Trident/"
            "Windows CE"
            # Missing contact information
            "efx-scanner/3.0"
            "Go-http-client/1.1"
          ];
      };
    };
  };
}
