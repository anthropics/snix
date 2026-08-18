let
  # `source1` and `source2` are fixed-output derivations
  #  with the same output hash but different builders. Different
  #  derivations, same hash modulo.
  source1 = builtins.derivation {
    name = "source";
    builder = "/bin/first";
    system = "x86_64-linux";
    outputHash = "sha256-Q3QXOoy+iN4VK2CflvRulYvPZXYgF0dO7FoF7CvWFTA=";
  };
  source2 = builtins.derivation {
    name = "source";
    builder = "/bin/second";
    system = "x86_64-linux";
    outputHash = "sha256-Q3QXOoy+iN4VK2CflvRulYvPZXYgF0dO7FoF7CvWFTA=";
  };

  # `intermediate1` and `intermediate2` each depend on one
  # of the sources, and have two outputs (`out` and `dev`). Again different
  # derivations, same hash modulo.
  intermediate1 = builtins.derivation {
    name = "intermediate";
    builder = "/bin/intermediate";
    system = "x86_64-linux";
    outputs = ["out" "dev"];
    source = source1;
  };
  intermediate2 = builtins.derivation {
    name = "intermediate";
    builder = "/bin/intermediate";
    system = "x86_64-linux";
    outputs = ["out" "dev"];
    source = source2;
  };

  # `parent-split` takes `out` from one intermediate and `dev` from the
  # other; `parent-joined` takes both outputs from a single one. Since
  # the intermediates are indistinguishable modulo, these two must have
  # the same hash modulo.
  parent-split = builtins.derivation {
    name = "parent";
    builder = "/bin/parent";
    system = "x86_64-linux";
    first = intermediate1.out;
    second = intermediate2.dev;
  };
  parent-join = builtins.derivation {
    name = "parent";
    builder = "/bin/parent";
    system = "x86_64-linux";
    first = intermediate1.out;
    second = intermediate1.dev;
  };
in [
  (parent-split.outPath == parent-join.outPath)
  (parent-split.drvPath != parent-join.drvPath)
]
