{ depot, ... }:

(depot.snix.crates.workspaceMembers.nar-bridge.build.override {
  runTests = true;
})
