{ depot, lib, ... }:

(depot.snix.crates.workspaceMembers.snix-cli-build.build.override {
  runTests = true;
})
