{
  depot,
  pkgs,
  lib,
  ...
}:

(depot.snix.crates.workspaceMembers.snix-castore.build.override {
  runTests = true;
  testPreRun = ''
    export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
  '';
}).overrideAttrs
  (old: rec {
    meta.ci.targets = [
      "integration-tests"
    ]
    ++ lib.filter (x: lib.hasPrefix "with-features" x || x == "no-features") (lib.attrNames passthru);
    passthru =
      (depot.snix.utils.mkFeaturePowerset {
        inherit (old) crateName;
        features = (
          [
            "cloud"
            "compat-migrate-directory-sizes"
            "fuse"
            "xp-composition-url-refs"
          ]
          # virtiofs feature currently fails to build on Darwin
          ++ lib.optional pkgs.stdenv.hostPlatform.isLinux "virtiofs"
        );
        override.testPreRun = ''
          export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
        '';
      })
      // {
        integration-tests = depot.snix.crates.workspaceMembers.${old.crateName}.build.override (old: {
          runTests = true;
          testPreRun = ''
            export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
            export PATH="$PATH:${
              pkgs.lib.makeBinPath [
                pkgs.cbtemulator
                pkgs.google-cloud-bigtable-tool
              ]
            }"
          '';
          features = old.features ++ [ "integration" ];
        });
      };
  })
