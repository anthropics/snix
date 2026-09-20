{
  depot,
  pkgs,
  ...
}:
let
  crates = depot.contrib.nix-language-test-suite.src.crates.override {
    # Build suite dependencies in debug profile to
    # keep debug assertions of snix crates.
    release = false;
  };
in
crates.workspaceMembers.nix-language-test-suite-snix.build.override {
  runTests = true;
  testCrateFlags = [ "--nocapture" ];
  testPreRun = ''
    export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
  '';
}
