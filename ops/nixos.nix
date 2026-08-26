# Helper functions for instantiating depot-compatible NixOS machines.
{
  depot,
  lib,
  pkgs,
  ...
}@args:

let
  inherit (lib) findFirst;
in
rec {
  # This provides our standard set of arguments to all NixOS modules.
  baseModule =
    { ... }:
    {
      nix.nixPath =
        let
          # Due to nixpkgsBisectPath, pkgs.path is not always in the nix store
          nixpkgsStorePath =
            if lib.hasPrefix builtins.storeDir (toString pkgs.path) then
              builtins.storePath pkgs.path # nixpkgs is already in the store
            else
              pkgs.path; # we need to dump nixpkgs to the store either way
        in
        [
          ("nixos=" + nixpkgsStorePath)
          ("nixpkgs=" + nixpkgsStorePath)
        ];

      nixpkgs.pkgs = pkgs;
    };

  nixosFor =
    configuration:
    (import (depot.third_party.sources.nixpkgs + "/nixos") {
      configuration =
        { ... }:
        {
          imports = [
            baseModule
            configuration
          ];
        };

      specialArgs = {
        inherit (args) depot;
      };
    });

  findSystem =
    hostname:
    (findFirst (
      system: system.config.networking.hostName == hostname
    ) (throw "${hostname} is not a known NixOS host") (map nixosFor depot.ops.machines.all-systems));

  # Systems that should be built in CI
  archivistEC2System = (nixosFor depot.ops.machines.archivist-ec2).system;
  build03System = (nixosFor depot.ops.machines.build03).system;
  gerrit01System = (nixosFor depot.ops.machines.gerrit01).system;
  meta01System = (nixosFor depot.ops.machines.meta01).system;
  public01System = (nixosFor depot.ops.machines.public01).system;
  snixCacheSystem = (nixosFor depot.ops.machines.snix-cache).system;

  meta.ci.targets = [
    "archivistEC2System"
    "gerrit01System"
    "meta01System"
    "public01System"
    "snixCacheSystem"
  ];
}
