{
  pkgs,
  lib,
  depot,
  ...
}:

let
  # Filters the given source, only keeping files related to the build, preventing unnecessary rebuilds.
  # Includes src in the root, all other .rs files and optionally Cargo specific files.
  # Additional files to be included can be specified in extraFileset.
  filterRustCrateSrc =
    {
      root, # The original src
      extraFileset ? null, # Additional filesets to include (e.g. fileFilter for proto files)
      cargoSupport ? false,
    }:
    lib.fileset.toSource {
      inherit root;
      fileset =
        lib.fileset.intersection (lib.fileset.fromSource root) # We build our final fileset from the original src
          (
            lib.fileset.unions (
              [
                (lib.fileset.maybeMissing (root + "/src")) # src may be missing if the crate just has tests for example
                (lib.fileset.fileFilter (f: f.hasExt "rs") root)
              ]
              ++ lib.optionals cargoSupport [
                (lib.fileset.fileFilter (f: f.name == "Cargo.toml") root)
                (lib.fileset.maybeMissing (root + "/Cargo.lock"))
              ]
              ++ lib.optional (extraFileset != null) extraFileset
            )
          );
    };

in
{
  mkFeaturePowerset =
    {
      crateName,
      features,
      override ? { },
    }:
    let
      powerset =
        xs:
        let
          addElement = set: element: set ++ map (e: [ element ] ++ e) set;
        in
        lib.foldl' addElement [ [ ] ] xs;
    in
    lib.listToAttrs (
      map (features: {
        name =
          if features != [ ] then "with-features-${lib.concatStringsSep "-" features}" else "no-features";
        value =
          (depot.snix.crates.workspaceMembers.${crateName}.build.override (
            old:
            let
              attrs = {
                runTests = true;
                inherit features;
              };
            in
            attrs // (if lib.isFunction override then override (old // attrs) else override)
          )).overrideAttrs
            ({
              # Ensure that powerset CI steps run with low priority
              meta.ci.buildkiteExtraStepArgs.priority = -100;
            });
      }) (powerset features)
    );

  inherit filterRustCrateSrc;

  # A function which takes a pkgs instance and returns an overriden defaultCrateOverrides with support for snix crates.
  # This can be used throughout the rest of the repo.
  defaultCrateOverridesForPkgs =
    pkgs:
    pkgs.defaultCrateOverrides
    // {
      nar-bridge = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      nix-compat = prev: {
        src = filterRustCrateSrc rec {
          root = prev.src.origSrc;
          extraFileset = root + "/testdata";
        };
      };

      nix-compat-derive = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      nix-compat-derive-tests = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      nix-daemon = prev: {
        src = depot.snix.utils.filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-build = prev: {
        src = filterRustCrateSrc rec {
          root = prev.src.origSrc;
          extraFileset = lib.fileset.fileFilter (f: f.hasExt "proto") root;
        };
        PROTO_ROOT = depot.snix.build.protos.protos;
        nativeBuildInputs = [ pkgs.protobuf ];
        SNIX_BUILD_SANDBOX_SHELL =
          if pkgs.stdenv.hostPlatform.isLinux then pkgs.pkgsStatic.busybox + "/bin/sh" else "/bin/sh";
      };

      snix-build-glue = prev: {
        src = filterRustCrateSrc rec {
          root = prev.src.origSrc;
          extraFileset = root + "/test-data";
        };
      };

      snix-castore = prev: {
        src = filterRustCrateSrc rec {
          root = prev.src.origSrc;
          extraFileset = lib.fileset.fileFilter (f: f.hasExt "proto") root;
        };
        PROTO_ROOT = depot.snix.castore.protos.protos;
        nativeBuildInputs = [ pkgs.protobuf ];
      };

      snix-castore-http = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-cli = prev: {
        src = filterRustCrateSrc rec {
          root = prev.src.origSrc;
          extraFileset = root + "/tests";
        };
      };

      snix-store = prev: {
        src = filterRustCrateSrc rec {
          root = prev.src.origSrc;
          extraFileset = lib.fileset.fileFilter (f: f.hasExt "proto") root;
        };
        PROTO_ROOT = depot.snix.store.protos.protos;
        nativeBuildInputs = [ pkgs.protobuf ];
      };

      snix-eval-builtin-macros = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-eval = prev: {
        src = filterRustCrateSrc rec {
          root = prev.src.origSrc;
          extraFileset = root + "/proptest-regressions";
        };
      };

      snix-glue = prev: {
        src = filterRustCrateSrc {
          root = prev.src.origSrc;
        };
      };

      snix-serde = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-tracing = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-cli-build = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-cli-castore = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-cli-castore-http = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-cli-derivation-show = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-cli-eval = prev: {
        src = filterRustCrateSrc rec {
          root = prev.src.origSrc;
          extraFileset = lib.fileset.fileFilter (f: f.hasExt "nix") (root + "/tests");
        };
      };

      snix-cli-nar-bridge = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-cli-nix-daemon = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };

      snix-cli-store = prev: {
        src = filterRustCrateSrc { root = prev.src.origSrc; };
      };
    };

  mkCrate2nixFastCheck =
    path: # The path to the Cargo.nix to be checked.
    let
      crate2nix-check = depot.snix.utils.mkCrate2nixCheck path;
    in
    crate2nix-check.command.overrideAttrs {
      meta.ci.extraSteps = {
        inherit crate2nix-check;
      };
      meta.ci.fast = true;
    };

  # This creates an extraStep in CI to check whether the Cargo.nix file is up-to-date.
  mkCrate2nixCheck =
    path: # The path to the Cargo.nix to be checked.
    let
      relCrateRoot = lib.removePrefix "./" (
        builtins.dirOf (lib.path.removePrefix depot.path.origSrc path)
      );
    in
    {
      label = "crate2nix check for ${relCrateRoot}";
      needsOutput = true;
      alwaysRun = true;
      command = pkgs.writeShellScript "crate2nix-check-for-${lib.replaceStrings [ "/" ] [ "-" ] relCrateRoot}" ''
        (cd $(git rev-parse --show-toplevel)/${relCrateRoot} &&
          ${depot.tools.crate2nix-generate}/bin/crate2nix-generate &&
          if [[ -n "$(git status --porcelain -unormal Cargo.nix Cargo.lock)" ]]; then
              echo "----------------------------------------------------------------------------------------------------"
              echo "Cargo.nix or Cargo.lock needs to be updated, run 'mg run //tools/crate2nix-generate' in ${relCrateRoot}"
              echo "----------------------------------------------------------------------------------------------------"
              exit 1
          fi
        )
      '';
    };
}
