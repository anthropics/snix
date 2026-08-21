{
  depot,
  pkgs,
  lib,
  ...
}:

let
  mkImportCheck = p: expectedPath: {
    label = ":nix :import ${p} with snix-store import";
    needsOutput = true;
    command = pkgs.writeShellScript "snix-import-check" ''
      export BLOB_SERVICE_ADDR=memory:
      export DIRECTORY_SERVICE_ADDR=redb+memory:
      export PATH_INFO_SERVICE_ADDR=redb+memory:
      SNIX_STORE_OUTPUT=$(result/bin/snix-store import-path ${p})
      EXPECTED='${
        # the vebatim expected Snix output:
        expectedPath
      }'

      echo "snix-store output: ''${SNIX_STORE_OUTPUT}"
      if [ "$SNIX_STORE_OUTPUT" != "$EXPECTED" ]; then
        echo "Correct would have been ''${EXPECTED}"
        exit 1
      fi

      echo "Output was correct."
    '';
  };
in

(depot.snix.crates.workspaceMembers.snix-cli-store.build.override (old: {
  runTests = true;
})).overrideAttrs
  (old: rec {
    meta.ci = {
      targets = lib.filter (x: lib.hasPrefix "with-features" x || x == "no-features") (
        lib.attrNames passthru
      );
      extraSteps.import-website = (mkImportCheck "web/content" ../../../web/content);
    };
    passthru =
      old.passthru
      // (depot.snix.utils.mkFeaturePowerset {
        inherit (old) crateName;
        features = (
          [
            "fuse"
            "xp-store-composition-cli"
          ]
          # virtiofs feature currently fails to build on Darwin
          ++ lib.optional pkgs.stdenv.isLinux "virtiofs"
        );
        override =
          old:
          old
          // {
            features = old.features ++ [
              "compat-migrate-directory-sizes"
              "otlp"
              "tonic-reflection"
              "tracing-chrome"
              "tracing-tracy"
            ];
          };
      });
  })
