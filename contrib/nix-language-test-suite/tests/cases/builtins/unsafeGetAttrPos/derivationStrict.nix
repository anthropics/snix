builtins.unsafeGetAttrPos "drvPath" (derivationStrict {
  name = "foo";
  builder = "/bin/sh";
  system = "x86_64-linux";
})
