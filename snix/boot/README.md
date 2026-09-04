# snix/boot

This directory provides tooling to boot VMs with `/nix/store` provided by
virtiofs from a snix-store. Integration tests live in the `tests/` subdirectory.

## //snix/boot:run-snix-vm
A script spinning up a `snix-store virtiofs` daemon, then booting a cloud-
hypervisor VM whose `/nix/store` is mounted from it.

It supports the following env vars:
 - `CH_NUM_CPUS=2` controls the number of CPUs available to the VM
 - `CH_MEM_SIZE=512M` controls the memory available to the VM
 - `CH_CMDLINE=` is appended to the kernel cmdline (use it to set `init=`)

The VM boots a NixOS kernel and initrd using systemd-initrd, which mounts
`/nix/store` from virtiofs. What runs is selected via `init=` on the kernel
cmdline (set through `CH_CMDLINE`):

 - `init=/nix/store/…-nixos-system-…/init` boots a full NixOS system,
 - `init=/nix/store/…/bin/some-binary` runs an arbitrary binary (can be a
   shell too).

The store path referenced by `init=` (and its closure) must be present in the
snix-store, so it can be served over virtiofs.

### Usage
Build `snix-store` and put it on `$PATH` — `run-snix-vm` calls it from there.
It needs the non-default `virtiofs` feature. From the `snix` directory:

```
cargo build -p snix-cli-store --features virtiofs
export PATH=$PATH:$PWD/target/debug
```

Point snix at some (local) stores. Both `snix-store upload` and the `snix-store
virtiofs` daemon (used by `run-snix-vm`) read these env vars and open the
services directly:

```
export BLOB_SERVICE_ADDR=objectstore+file://$PWD/blobs
export DIRECTORY_SERVICE_ADDR=redb:$PWD/directories.redb
export PATH_INFO_SERVICE_ADDR=redb:$PWD/pathinfo.redb
```

Copy paths (and their closures) into the store with `snix-store upload
reference-graph`. Define a helper:

```
copy() {
  nix --extra-experimental-features nix-command \
    path-info --json --recursive "$1" | snix-store upload reference-graph -
}
```

Build the runner once (`-A` resolves against the repo root, one level up):

```
nix-build .. -A snix.boot.run-snix-vm   # creates ./result/bin/run-snix-vm
```

#### Execute a specific binary
Copy a binary (and its closure) into the snix-store, then point `init=` at it:

```
hello=$(nix-build --no-out-link .. -A third_party.nixpkgs.hello)
copy "$hello"
CH_CMDLINE="init=$hello/bin/hello" ./result/bin/run-snix-vm
```

As `init=` runs as PID 1, the kernel panics once the binary exits, which (with
`panic=-1`) reboots and powers the VM off.

##### Interactive shell
```
shell=$(nix-build --no-out-link .. -A third_party.nixpkgs.bashInteractive)
copy "$shell"
CH_CMDLINE="init=$shell/bin/sh" ./result/bin/run-snix-vm
```

You'll get a shell with `/nix/store` mounted read-only. `coreutils` isn't on
`$PATH`, but bash builtins work — e.g. `echo /nix/store/*` lists the store.

#### Boot a NixOS system closure
Booting a full NixOS system works the same way, by pointing `init=` at the
system's /init.

It currently uses the same initrd and kernel as all other images,
and does not honor any configuration in your system configuration (FUTUREWORK).

Your NixOS system configuration should import `"${modulesPath}/profiles/qemu-guest.nix"`.

Copy the system closure into the snix-store (as above), then:

```
CH_CMDLINE=init=/nix/store/…-nixos-system-…/init ./result/bin/run-snix-vm
```
