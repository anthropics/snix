{
  depot,
  pkgs,
  lib,
  ...
}:
lib.makeOverridable (
  {
    pname,
    features ? [ ],
    allFeatures ? false,
    usesDefaultFeatures ? true,
    paths,
    base,
  }:
  let
    mkFeatured =
      package:
      let
        all-features = lib.attrNames depot.snix.crates.internal.crates.${package.crateName}.features or { };
        os-features =
          if pkgs.stdenv.hostPlatform.isDarwin then lib.remove "virtiofs" all-features else all-features;
        reduced-features = lib.remove "tracing-chrome" (lib.remove "tracy" os-features);
      in
      package.override (old: {
        features =
          if allFeatures then
            reduced-features
          else
            lib.intersectLists (
              if usesDefaultFeatures then old.features or [ ] ++ features else features
            ) os-features;
      });
    libexec = pkgs.buildEnv {
      name = "snix-cli-libexec-${pname}";
      paths = lib.map mkFeatured paths;
      pathsToLink = [
        "/bin"
      ];
      postBuild = ''
        mv $out/bin $out/libexec
      '';
    };
    base_ = mkFeatured base;
  in
  pkgs.runCommand "snix-cli-${pname}"
    {
      nativeBuildInputs = [ pkgs.makeWrapper ];
    }
    ''
      mkdir -p $out/bin
      cp -a ${libexec}/* $out/
      makeWrapper ${base_}/bin/snix \
        $out/bin/snix \
        --suffix SNIX_LIBEXEC_PATH : $out/libexec
    ''
)
