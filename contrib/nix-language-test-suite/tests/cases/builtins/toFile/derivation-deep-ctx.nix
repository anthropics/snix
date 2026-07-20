let
  drv = builtins.derivation {
    name = "foo";
    builder = ":";
    system = ":";
  };
in
  # A derivation-deep context element (`{ allOutputs = true; }`) is not allowed.
  builtins.toFile "foo" drv.drvPath
