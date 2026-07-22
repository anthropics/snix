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
