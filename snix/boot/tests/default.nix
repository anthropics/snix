{
  depot,
  pkgs,
  lib,
  ...
}:

let
  inherit (depot.snix.boot) snixGuest run-snix-vm;

  # `init=` payload for the "list the store" tests: print the top-level store
  # paths, then power off. This runs as the switch-root target (PID 1), so it
  # uses absolute paths, writes to the console explicitly, and powers off via
  # the reboot(2) syscall (busybox).
  listStoreInit = pkgs.writeScript "snix-list-store" ''
    #!${pkgs.busybox}/bin/sh
    exec >/dev/console 2>&1
    ${pkgs.busybox}/bin/find /nix/store -maxdepth 1
    ${pkgs.busybox}/bin/poweroff -f
  '';

  # Seed a snix-store with the specified path, then boot a VM (the regular NixOS
  # kernel + systemd-initrd) against it. systemd-initrd mounts /nix/store from
  # virtiofs; the kernel cmdline (`init=`) selects what to run afterwards.
  #
  # Allows customizing the cmdline, which can be used to list files, or boot
  # a NixOS system (`init=…/init`).
  mkBootTest =
    {
      # Test name.
      name,

      blobServiceAddr ? "memory:",
      directoryServiceAddr ? "redb+memory:",
      pathInfoServiceAddr ? "redb+memory:",

      # The path to seed into the snix-store (and, by default, assert on).
      path,

      # Whether to use nar-bridge to upload, rather than snix-store upload.
      # using nar-bridge currently is "slower", as the `pkgs.mkBinaryCache` build
      # takes quite some time.
      useNarBridge ? false,

      # Commands to run before starting the snix-daemon. Useful to provide
      # auxiliary mock services.
      preStart ? "",

      # The kernel cmdline to pass to the VM. Selects what to run once
      # /nix/store is mounted.
      bootCmdline,
      # Additional store paths whose closures to copy into the snix-store
      # alongside the booted `path` — e.g. content to assert gets served over
      # virtiofs.
      extraPaths ? [ ],

      # The string we expect to find in the VM output.
      # Defaults the value of `path` (the store path we upload).
      assertVMOutput ? path,
    }:

    pkgs.stdenv.mkDerivation (
      {
        name = "test-boot-${name}";

        nativeBuildInputs = [
          depot.snix.cli.store.with-features-virtiofs
          run-snix-vm
        ]
        ++ lib.optionals (!useNarBridge) [
          pkgs.jq
        ]
        ++ lib.optionals useNarBridge [
          depot.snix.cli.nar-bridge
          pkgs.curl
          pkgs.rush-parallel
          pkgs.zstd.bin
          pkgs.nix
        ];
        buildCommand = ''
          set -eou pipefail
          touch $out
          # Ensure we can construct http clients.
          export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt

          ${preStart}

          # Start the snix daemon, listening on a unix socket.
          BLOB_SERVICE_ADDR=${lib.escapeShellArg blobServiceAddr} \
          DIRECTORY_SERVICE_ADDR=${lib.escapeShellArg directoryServiceAddr} \
          PATH_INFO_SERVICE_ADDR=${lib.escapeShellArg pathInfoServiceAddr} \
            snix-store daemon -l $PWD/snix-store.sock &

          # Wait for the service to report healthy.
          timeout 22 sh -c "until ${pkgs.grpc-health-probe}/bin/grpc-health-probe -addr unix://$PWD/snix-store.sock; do sleep 1; done"

          # Export env vars so that subsequent snix-store commands will talk to
          # our snix store daemon over the unix socket.
          export BLOB_SERVICE_ADDR=grpc+unix:$PWD/snix-store.sock
          export DIRECTORY_SERVICE_ADDR=grpc+unix:$PWD/snix-store.sock
          export PATH_INFO_SERVICE_ADDR=grpc+unix:$PWD/snix-store.sock
        ''
        + lib.optionalString (!useNarBridge) ''
          echo "Copying closure ${path}…"
          jq .closure < $NIX_ATTRS_JSON_FILE | snix-store upload reference-graph -qqq -
        ''
        + lib.optionalString useNarBridge ''
          echo "Starting nar-bridge…"
          snix-nar-bridge -l $PWD/nar-bridge.sock &

          # Wait for nar-bridge to report healthy.
          timeout 22 sh -c "until ${pkgs.curl}/bin/curl -s --unix-socket $PWD/nar-bridge.sock http:///nix-binary-cache; do sleep 1; done"

          # Upload. We can't use nix copy --to http://…, as it wants access to the nix db.
          # However, we can use mkBinaryCache to assemble .narinfo and .nar.xz to upload,
          # and then drive a HTTP client ourselves.
          to_upload=${
            pkgs.mkBinaryCache {
              rootPaths = [ path ];
              # Implemented in https://github.com/NixOS/nixpkgs/pull/376365
              compression = "zstd";
            }
          }

          # Upload all NAR files (with some parallelism).
          # As mkBinaryCache produces them zstd-compressed, unpack them on the fly.
          # nar-bridge doesn't care about the path we upload *to*, but a
          # subsequent .narinfo upload need to refer to its contents (by narhash).
          echo -e "Uploading NARs… "
          # TODO(flokli): extension of the nar files where changed from .nar.{compression} to .{compression}
          # https://github.com/NixOS/nixpkgs/pull/376365
          ls -d $to_upload/nar/*.zst | rush -n1 'nar_hash=$(zstdcat < {} | nix-hash --base32 --type sha256 --flat /dev/stdin);zstdcat < {} | curl -s --fail-with-body -T - --unix-socket $PWD/nar-bridge.sock http://localhost:9000/nar/''${nar_hash}.nar'
          echo "Done."

          # Upload all NARInfo files.
          # FUTUREWORK: This doesn't upload them in order, and currently relies
          # on PathInfoService not doing any checking.
          # In the future, we might want to make this behaviour configurable,
          # and disable checking here, to keep the logic simple.
          ls -d $to_upload/*.narinfo | rush 'curl -s -T - --unix-socket $PWD/nar-bridge.sock http://localhost:9000/$(basename {}) < {}'
        ''
        + ''
          # Invoke a VM using snix as the backing store, ensure the outpath appears in its listing.
          echo "Starting VM…"

          # Bound the VM runtime so a failed boot (e.g. dropping into an initrd
          # emergency shell) fails the test instead of hanging forever.
          CH_CMDLINE="${bootCmdline}" timeout 300 run-snix-vm 2>&1 | tee output.txt
          grep "${assertVMOutput}" output.txt
        '';
        requiredSystemFeatures = [ "kvm" ];
        # HACK: The boot tests are sometimes flaky, and we don't want them to
        # periodically fail other build. Have Buildkite auto-retry them 2 times
        # on failure.
        # Logs for individual failures are still available, so it won't hinder
        # flakiness debuggability.
        meta.ci.buildkiteExtraStepArgs = {
          retry.automatic = true;
        };
      }
      // lib.optionalAttrs (!useNarBridge) {
        __structuredAttrs = true;
        exportReferencesGraph.closure = [ path ] ++ extraPaths;
      }
    );

  # A NixOS system bootable from the snix-store, used by the closure-* tests.
  testSystem =
    (pkgs.nixos {
      imports = [ snixGuest ];

      services.getty.helpLine = "Onwards and upwards.";
      systemd.services.do-shutdown = {
        after = [ "getty.target" ];
        description = "Shut down again";
        wantedBy = [ "multi-user.target" ];
        serviceConfig.Type = "oneshot";
        script = "/run/current-system/sw/bin/systemctl poweroff --when=+10s";
      };
    }).config.system.build.toplevel;

in
depot.nix.readTree.drvTargets {
  website-memory = (
    mkBootTest {
      name = "website-memory";
      path = listStoreInit;
      extraPaths = [ ../../../web/content ];
      bootCmdline = "init=${listStoreInit}";
      assertVMOutput = ../../../web/content;
    }
  );

  website-persistent = (
    mkBootTest {
      name = "website-persistent";
      blobServiceAddr = "objectstore+file:/build/blobs";
      directoryServiceAddr = "redb:/build/directories.redb";
      pathInfoServiceAddr = "redb:/build/pathinfo.redb";
      path = listStoreInit;
      extraPaths = [ ../../../web/content ];
      bootCmdline = "init=${listStoreInit}";
      assertVMOutput = ../../../web/content;
    }
  );

  closure-snix = (
    mkBootTest {
      name = "closure-snix";
      blobServiceAddr = "objectstore+file:/build/blobs";
      path = listStoreInit;
      extraPaths = [ depot.snix.store ];
      bootCmdline = "init=${listStoreInit}";
      assertVMOutput = depot.snix.store;
    }
  );

  closure-nixos = (
    mkBootTest {
      name = "closure-nixos";
      blobServiceAddr = "objectstore+file:/build/blobs";
      directoryServiceAddr = "redb:/build/directories.redb";
      pathInfoServiceAddr = "redb:/build/pathinfo.redb";
      path = testSystem;
      bootCmdline = "init=${testSystem}/init";
      assertVMOutput = "Onwards and upwards.";
    }
  );

  closure-nixos-bigtable = (
    mkBootTest {
      name = "closure-nixos-bigtable";
      blobServiceAddr = "objectstore+file:/build/blobs";
      directoryServiceAddr = "bigtable://instance-1?project_id=project-1&table_name=directories&family_name=cf1";
      pathInfoServiceAddr = "bigtable://instance-1?project_id=project-1&table_name=pathinfos&family_name=cf1";
      useNarBridge = true;
      preStart = ''
        ${pkgs.cbtemulator}/bin/cbtemulator -address $PWD/cbtemulator.sock &
        timeout 22 sh -c 'until [ -e $PWD/cbtemulator.sock ]; do sleep 1; done'

        export BIGTABLE_EMULATOR_HOST=unix://$PWD/cbtemulator.sock
        ${pkgs.google-cloud-bigtable-tool}/bin/cbt -instance instance-1 -project project-1 createtable directories
        ${pkgs.google-cloud-bigtable-tool}/bin/cbt -instance instance-1 -project project-1 createfamily directories cf1
        ${pkgs.google-cloud-bigtable-tool}/bin/cbt -instance instance-1 -project project-1 createtable pathinfos
        ${pkgs.google-cloud-bigtable-tool}/bin/cbt -instance instance-1 -project project-1 createfamily pathinfos cf1
      '';
      path = testSystem;
      bootCmdline = "init=${testSystem}/init";
      assertVMOutput = "Onwards and upwards.";
    }
  );

  closure-nixos-s3 = (
    mkBootTest {
      name = "closure-nixos-s3";
      blobServiceAddr = "objectstore+s3://mybucket/blobs?aws_access_key_id=myaccesskey&aws_secret_access_key=supersecret&aws_endpoint_url=http%3A%2F%2Flocalhost%3A8333&aws_allow_http=1";
      # we cannot use s3 here yet without any caching layer, as we don't allow "deeper" access to directories (non-root nodes)
      # directoryServiceAddr = "objectstore+s3://mybucket/directories?aws_access_key_id=myaccesskey&aws_secret_access_key=supersecret&endpoint=http%3A%2F%2Flocalhost%3A8333&aws_allow_http=1";
      directoryServiceAddr = "redb+memory:";
      pathInfoServiceAddr = "redb+memory:";
      preStart = ''
        # start seaweedfs and wait for it to be ready
        AWS_ACCESS_KEY_ID=myaccesskey AWS_SECRET_ACCESS_KEY=supersecret S3_ENDPOINT=http://localhost:8333 ${pkgs.seaweedfs}/bin/weed mini -dir=$(mktemp -d) -s3 &
        timeout 22 sh -c "until ${pkgs.curl}/bin/curl --silent 127.0.0.1:8333/healthz; do sleep 1; done"

        # create our bucket
        AWS_ACCESS_KEY_ID=myaccesskey AWS_SECRET_ACCESS_KEY=supersecret ${pkgs.awscli2}/bin/aws --endpoint-url http://localhost:8333 s3 mb s3://my-bucket
        echo "healthy, bucket created"
      '';
      path = testSystem;
      bootCmdline = "init=${testSystem}/init";
      assertVMOutput = "Onwards and upwards.";
    }
  );

  closure-nixos-nar-bridge = (
    mkBootTest {
      name = "closure-nixos-nar-bridge";
      blobServiceAddr = "objectstore+file:/build/blobs";
      useNarBridge = true;
      path = testSystem;
      bootCmdline = "init=${testSystem}/init";
      assertVMOutput = "Onwards and upwards.";
    }
  );
}
