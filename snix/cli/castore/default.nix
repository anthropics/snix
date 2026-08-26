{
  depot,
  pkgs,
  lib,
  ...
}:

(depot.snix.crates.workspaceMembers.snix-cli-castore.build.override {
  runTests = true;
}).overrideAttrs
  (old: rec {
    meta.ci.targets = lib.filter (x: lib.hasPrefix "with-features" x || x == "no-features") (
      lib.attrNames passthru
    );
    passthru = (
      depot.snix.utils.mkFeaturePowerset {
        inherit (old) crateName;
        features = (
          [
            "fuse"
            "tonic-reflection"
            "xp-store-composition-cli"
          ]
          # virtiofs feature currently fails to build on Darwin
          ++ lib.optional pkgs.stdenv.hostPlatform.isLinux "virtiofs"
        );
      }
    );
  })
