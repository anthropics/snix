---
title: "Deploy your own binary cache"
slug: binary-cache
description: "Describes how to deploy snix to provide a binary cache"
summary: ""
date: 2026-08-05T12:00:33+00:00
lastmod: 2026-08-05T12:00:33+00:00
draft: false
weight: 15
toc: true
---

This describes how to use snix as a binary cache (for CI workloads).

It internally uses `snix-castore` for store path contents (making use of its deduplication properties to require less storage), and exposes it both as a Nix HTTP Binary cache frontend (to be used by Nix) and via the Snix gRPC protocol (to be used by Snix).

The setup consists of three components:

 - nginx, as a reverse proxy in front of:
 - `snix-store daemon` as a gRPC server accessing the stores directly
 - `nar-bridge` as a Nix HTTP Binary cache endpoint, connecting to `snix-store daemon` to retrieve data.

While it is theoretically possible to have `nar-bridge` open data stores directly without round-tripping via `snix-store daemon`, most stores can only be opened once.
We explicitly chose to separate this into two different processes so that you can still interact with `snix-[ca]store` while `nar-bridge` is running.

Having nginx in front also allows you to decide what to expose to the outside, potentially add an authentication layer and cache some endpoints.

## Deploying
To deploy `snix-store-daemon` and `nar-bridge`, you can use the modules provided at `ops/modules/{snix-store-daemon,nar-bridge}.nix`.

{{<callout>}}
This currently requires setting a `depot` attrset present in `specialArgs` when instantiating your NixOS system, but you can just set it to an empty attr, as long as you set `services.{snix-store-daemon,nar-bridge}.package` by yourself, pointing to the respective binaries.
{{</callout>}}

### snix-store-daemon
This example deploys a `snix-store-daemon.{service,socket}`, listening on `/run/snix-store-daemon.sock`.

It uses S3 for storing blobs, and redb for storing directories and pathinfos.
It uses the systemd credentials system to pass in an `AWS_CONFIG_FILE` pointing to a local S3 deployment, and providing some API credentials for it, but this would also work with instance credentials on AWS.

```nix
services.snix-store-daemon = {
  enable = true;
  settings = {
    blobservices.root = {
      type = "objectstore";
      object_store_url = "s3://snix-ci-cache/blobs";
      object_store_options = { };
    };

    directoryservices.root = {
      type = "redb";
      path = "/var/lib/snix-store/directories.redb";
    };

    pathinfoservices.root = {
      type = "redb";
      path = "/var/lib/snix-store/pathinfo.redb";
    };
  };
};
systemd.services.snix-store-daemon = {
  serviceConfig.LoadCredential = "aws_config_file:${config.age.secrets.ci-cache-bucket-credentials.path}";
  environment.AWS_CONFIG_FILE = "%d/aws_config_file";
};
```

### nar-bridge
`nar-bridge` connects to `/run/snix-store-daemon.sock`, and exposes a Nix Binary Cache HTTP endpoint, while talking to `snix-[ca]store` over gRPC.

The configuration is quite simple:

```nix
services.nar-bridge = {
  enable = true;
  settings = {
    blobservices.root = {
      type = "grpc";
      url = "grpc+unix:/run/snix-store-daemon.sock";
    };

    directoryservices.root = {
      type = "grpc";
      url = "grpc+unix:/run/snix-store-daemon.sock";
    };

    pathinfoservices.root = {
      type = "grpc";
      url = "grpc+unix:/run/snix-store-daemon.sock";
    };
  };
};
```

### nginx
Both of these services listen on local unix domain sockets.

To expose them, we will use nginx. The example below exposes the `snix-[ca]store` gRPC endpoints, as well as all read paths for nar-bridge (rendering NARInfos, NARs and nar-listings).
We don't expose the write path for nar-bridge, as we use snix gRPC for cache uploads. All write paths and otherwise costly requests require mTLS.

Note the example below also uses [nginx' support for `useGrpcErrorPages`][nginx-use-grpc-error-pages] merged into nixpkgs, so make sure your pin is past that commit.

Depending on your setup, you might also want to require mTLS for the read path, and/or different `$ssl_client_s_dn` matching logic.

```nix
let
  passToSnixStoreDaemonAll = {
    useGrpcErrorPages = true;
    extraConfig = ''
      grpc_pass unix:/run/snix-store-daemon.sock;
      grpc_buffer_size 1m;

      client_max_body_size 0;
    '';
  };
  passToSnixStoreDaemonTrusted = {
    useGrpcErrorPages = true;
    extraConfig = ''
      # Trusted endpoints need mTLS
      if ($ssl_client_verify != SUCCESS) {
        return 401;
      }

      # We only allow certain DNs to talk to it
      if ($ssl_client_s_dn != "CN=my-custom-cn") {
        return 401;
      }

      ${passToSnixStoreDaemonAll.extraConfig}
    '';
  };

in
{
  services.nginx.virtualHosts."cache.example.com" = {
    forceSSL = true;
    enableACME = true;
    extraConfig = ''
      ssl_client_certificate /run/secrets/key.pem;
      ssl_verify_client optional;
    '';

    locations = {
      "/" = {
        proxyPass = "http://unix:/run/nar-bridge.sock:/";
        extraConfig = ''
          # Restrict allowed HTTP methods
          limit_except GET HEAD {
            # nar bridge allows to upload nars via PUT
            deny all;
          }

          # Propagate content-encoding to the backend
          proxy_set_header Accept-Encoding $http_accept_encoding;

          # Enable CORS from everywhere, same as c.n.o
          add_header Access-Control-Allow-Origin *;
        '';
      };

      "/grpc.reflection.v1alpha.ServerReflection" = passToSnixStoreDaemonAll;
      "/grpc.reflection.v1.ServerReflection" = passToSnixStoreDaemonAll;

      "/snix.castore.v1.BlobService/Put" = passToSnixStoreDaemonTrusted;
      "/snix.castore.v1.BlobService/Read" = passToSnixStoreDaemonAll;
      "/snix.castore.v1.BlobService/Stat" = passToSnixStoreDaemonAll;
      "/snix.castore.v1.DirectoryService/Get" = passToSnixStoreDaemonAll;
      "/snix.castore.v1.DirectoryService/Put" = passToSnixStoreDaemonTrusted;

      "/snix.store.v1.PathInfoService/CalculateNAR" = passToSnixStoreDaemonTrusted;
      "/snix.store.v1.PathInfoService/Get" = passToSnixStoreDaemonAll;
      "/snix.store.v1.PathInfoService/List" = passToSnixStoreDaemonTrusted;
      "/snix.store.v1.PathInfoService/Put" = passToSnixStoreDaemonTrusted;
    };
  };
}
```

### Uploads
How you want to upload highly depends on your CI setup. You might do this in a post-build hook, in some store watcher, or as a separate CI step after builds are done.

#### Using `nix copy`
You can use `nix copy` or similar tools to copy to the nar-bridge HTTP endpoint. This obviously requires the `GET` and `PUT` endpoints to be reachable from the host running that command.

In this example, we will use ssh port forwarding:

```console
$ ssh -L 8080:/run/nar-bridge.sock root@cache.example.com
$ nix copy --to http://localhost:8080?compression=none&secret-key=/run/secrets/nix-signing.key /nix/store/xxxx-some-store-path
```

#### Using `snix-store copy`
Above is always sending the entire NAR to nar-bridge, letting deduplication happen on the host running `nar-bridge` and `snix-store daemon`. Data already present in the stores is not inserted multiple times, but we still send lots of unnecessary data.

This obviously is suboptimal, so we will instead switch to the `snix-store copy` command and copy to the `gRPC` endpoint directly, which will avoid uploading data already present.

To use `snix-store copy`, we first write a composition config that implements signing:

```toml
[blobservices.root]
type = "grpc"
url = "grpc+https://cache.example.com?tls-client-cert-path=/run/secrets/cert.pem&tls-client-key-path=/run/secrets/key.pem"

[directoryservices.root]
type = "grpc"
url = "grpc+https://cache.example.com?tls-client-cert-path=/run/secrets/cert.pem&tls-client-key-path=/run/secrets/key.pem"

[pathinfoservices.grpc]
type = "grpc"
url = "grpc+https://cache.example.com?tls-client-cert-path=/run/secrets/cert.pem&tls-client-key-path=/run/secrets/key.pem"

[pathinfoservices.root]
inner = "&grpc"
keyfile = "/run/secrets/nix-signing.key"
type = "keyfile-signing"
```

This uses mTLS to authenticate when connecting to the gRPC endpoint.
If your server uses a custom CA, you might want to also set `tls-ca-cert-path`.

You can then run `snix-store copy` pointed to this config. Instead of the store paths to upload, it takes a path to a json file usually produced by the `nix path-info --json` command.

If set to `-`, it's read from stdin, so something like the following would work:

```console
$ EXPERIMENTAL_STORE_COMPOSITION=/path/to/snix-copy.toml \
  nix path-info --recursive --json /nix/store/xxxx-mypath | \
  snix-store copy -
```

## Caveats
Deploying this comes with a few caveats. We plan to improve things here over time, and will also update that list. Also, if you want to work on any of these, reach out!

### Slow NAR rendering
For bigger blobs, NAR rendering currently is a bit slower than it could be, due to [#93][cs-bs-split].
It can be alleviated by adding a caching layer, as done in [the nixos.snix.store deployment](https://git.snix.dev/snix/snix/src/commit/4b0d7f71d531444ac24b5890ab92d5f4f86ebfbb/ops/modules/www/nixos.snix.store.nix#L54).

### No garbage collection
There currently is no garbage collection implemented. While using snix-castore reduces storage requirement quite significantly when ingesting similar store paths over and over again, at some point you might need to reduce storage.

In case you're just using this as a CI cache, and you can afford to just delete all store paths, simply delete the snix backing storage and start with an empty state.

In case you need to retain certain store paths and their dependencies, consider copying these "pins" elsewhere, empty the storage, then copy them back in (for example substituting them to your local store and re-uploading after deleting the state).

Implementing proper GC is on the roadmap, but due to the different backends involved and transitive dependencies slightly more involved, requiring some planning. Reach out if you want to help designing this!

[cs-bs-split]: https://git.snix.dev/snix/snix/issues/93
[nginx-use-grpc-error-pages]: https://github.com/NixOS/nixpkgs/pull/549553
[^nar-compression]: Note that even though the NARInfo does say `Compression: none`, the actual NAR is sent compressed using zstd over the wire, by making use of HTTP's `Content-Encoding` header. Nix uses libcurl under the hood, which will transparently negotiate zstd compression.
