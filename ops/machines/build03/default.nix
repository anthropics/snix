{
  depot,
  lib,
  pkgs,
  ...
}: # readTree options
{ config, ... }: # passed by module system
let
  mod = name: depot.path.origSrc + ("/ops/modules/" + name);
in
{
  imports = [
    (mod "o11y/alloy.nix")
    (mod "snix-buildkite.nix")
    (mod "known-hosts.nix")

    ./snix-copy.nix
    ./watch-store.nix

    (depot.third_party.agenix.src + "/modules/age.nix")
  ];

  nixpkgs.hostPlatform = "x86_64-linux";

  boot = {
    loader.systemd-boot.enable = true;
    loader.efi.canTouchEfiVariables = true;
    kernelPackages = pkgs.linuxPackages_latest;
  };

  services.depot.buildkite = {
    enable = true;
    agentCount = 64;
    largeSlots = 32;
  };

  nix.nrBuildUsers = 256;
  nix.gc.automatic = true;
  nix.package = pkgs.lix;

  networking = {
    useNetworkd = true;
    useHostResolvConf = false;

    hostName = "build03";
    domain = "infra.snix.dev";
    nameservers = [
      "8.8.8.8"
      "8.8.4.4"
      "2001:4860:4860::8888"
      "2001:4860:4860::8844"
    ];

    nftables.enable = true;
    firewall.allowPing = true;
  };
  services.resolved.enable = true;
  services.resolved.settings.Resolve.DNSSEC = false;
  systemd.network.networks = {
    "10-uplink" = {
      matchConfig.Name = "en* eth*";
      DHCP = "no";
      addresses = [
        {
          Address = "116.202.234.220";
          Peer = "116.202.234.193";
        }
      ];
      gateway = [ "116.202.234.193" ];
      address = [
        "2a01:4f8:241:4de7::1/64"
      ];
      routes = [
        {
          Gateway = "fe80::1";
          GatewayOnLink = true;
        }
      ];
    };
  };

  fileSystems."/" = {
    device = "/dev/disk/by-label/root";
    fsType = "btrfs";
  };

  fileSystems."/boot" = {
    device = "/dev/disk/by-label/boot";
    fsType = "vfat";
  };

  age.secrets =
    let
      secretFile = name: depot.ops.secrets."${name}.age";
    in
    {
      buildkite-agent-token = {
        file = secretFile "buildkite-agent-token";
        mode = "0440";
        group = "buildkite-agents";
      };
      buildkite-private-key = {
        file = secretFile "buildkite-ssh-private-key";
        mode = "0440";
        group = "buildkite-agents";
      };
      buildkite-besadii-config = {
        file = secretFile "buildkite-besadii-config";
        mode = "0440";
        group = "buildkite-agents";
      };
      buildkite-graphql-token = {
        file = secretFile "buildkite-graphql-token";
        mode = "0440";
        group = "buildkite-agents";
      };
      mtls-private-key.file = secretFile "mtls-build03.infra.snix.dev";
    };

  systemd.tmpfiles.rules = [
    "d '/nix/var/nix/gcroots/buildkite' 0770 - buildkite-agents - -"
    "z '/nix/var/nix/gcroots' 0771 - - - -"
  ];

  services.openssh.enable = true;

  environment.systemPackages = with pkgs; [
    kitty.terminfo
  ];

  time.timeZone = "UTC";
  users.users.root.openssh.authorizedKeys.keys =
    depot.ops.users.edef ++ depot.ops.users.flokli ++ depot.ops.users.raito;
  users.groups.kvm = { };
  users.users.root.extraGroups = [ "kvm" ];

  system.stateVersion = "25.05";
}
