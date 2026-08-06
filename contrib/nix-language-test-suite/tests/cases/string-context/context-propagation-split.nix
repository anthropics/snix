let
  drv = derivation {
    name = "fail";
    builder = "/bin/false";
    system = "x86_64-linux";
    outputs = ["out"];
  };

  # `(.*)` always returns something like:
  #
  # [
  #   ""
  #   [ "<original string>" ]
  #   ""
  #   [ "" ]
  #   ""
  # ]
  #
  # So take the captured string and check its context
  noContext = str: builtins.hasContext (builtins.head (builtins.elemAt (builtins.split "(.*)" str) 1));
in [
  (noContext "foo")
  (noContext "${drv}")
]
