# This overlay is used to make TVL-specific modifications in the
# nixpkgs tree, where required.
{
  lib,
  depot,
  localSystem,
  ...
}:

final: prev:
depot.nix.readTree.drvTargets {
  crate2nix = prev.crate2nix.overrideAttrs (old: {
    patches = old.patches or [ ] ++ [
      # https://github.com/nix-community/crate2nix/pull/301
      ./patches/crate2nix-tests-debug.patch
    ];
  });

  evans = prev.evans.overrideAttrs (old: {
    patches = old.patches or [ ] ++ [
      # add support for unix domain sockets
      # https://github.com/ktr0731/evans/pull/680
      ./patches/evans-add-support-for-unix-domain-sockets.patch
    ];
  });

  keycloak = prev.keycloak.overrideAttrs (old: rec {
    version = "26.7.2";

    src = prev.fetchzip {
      url = "https://github.com/keycloak/keycloak/releases/download/${version}/keycloak-${version}.zip";
      hash = "sha256-D4Hj4OHX8veFjIDbvbQN0E7C2oHVpbD2U4TV1Z8fZ8Y=";
    };
  });

  # Use an old version of hugo, else the website only shows
  # "This line is from layouts/index.html."
  hugo = prev.hugo.overrideAttrs (old: {
    version = "0.145.0";

    src = prev.fetchFromGitHub {
      owner = "gohugoio";
      repo = "hugo";
      tag = "v0.145.0";
      hash = "sha256-5SV6VzNWGnFQBD0fBugS5kKXECvV1ZE7sk7SwJCMbqY=";
    };

    vendorHash = "sha256-aynhBko6ecYyyMG9XO5315kLerWDFZ6V8LQ/WIkvC70=";
  });

  watch-store = prev.callPackage ./pkgs/watch-store.nix { };
}
