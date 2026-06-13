let
  mapped = builtins.mapAttrs (name: value: value) {a = 1;};
  zipped = builtins.zipAttrsWith (name: values: values) [{a = 1;} {a = 3;}];
in [
  (builtins.unsafeGetAttrPos "a" mapped)
  (builtins.unsafeGetAttrPos "a" zipped)
]
