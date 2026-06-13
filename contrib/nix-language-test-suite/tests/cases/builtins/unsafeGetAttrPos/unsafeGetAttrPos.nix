let
  dynamic-attr = {
    "${"x"}" = 1;
  };
in [
  # Two cases below based on this commit:
  # https://github.com/NixOS/nix/commit/19ec1c9fd4d4bf6e941b046b8549ba2a1a690937
  #
  # Anonymous attrset
  (builtins.unsafeGetAttrPos "y" {y = "x";})

  # Attrset imported from another file
  (builtins.unsafeGetAttrPos "y" (import ./foo.nix))

  (builtins.unsafeGetAttrPos "x" dynamic-attr)
]
