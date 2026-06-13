let
  with-pos = {x = 1;};
  no-pos = builtins.mapAttrs (name: value: value) {x = 2;};
in [
  # update operator returns RHS position
  (builtins.unsafeGetAttrPos "x" ({x = 1;} // {x = 2;}))

  # so, if RHS has a position, it's inherited
  (builtins.unsafeGetAttrPos "x" (no-pos // with-pos))

  # but if RHS doesn't have a position, the updated
  # attrset won't have it too
  (builtins.unsafeGetAttrPos "x" (with-pos // no-pos))
]
