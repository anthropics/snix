# A derivation may have a single output whose name is not `out`.
(builtins.derivation {
  name = "single-non-out-output";
  builder = "/bin/sh";
  system = "x86_64-linux";
  outputs = [ "info" ];
}).drvPath
