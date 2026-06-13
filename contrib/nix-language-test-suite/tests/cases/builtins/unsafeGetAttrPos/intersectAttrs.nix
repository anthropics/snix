let
  lhs = {foo = 1;};
  rhs = {foo = 2; bar = 2;};
in [
  # intersectAttrs preserves position
  (builtins.unsafeGetAttrPos "foo" (builtins.intersectAttrs lhs rhs))

  # intersectAttrs preserves RHS position
  (builtins.unsafeGetAttrPos "foo" (builtins.intersectAttrs {foo = 1;} {foo = 2;}))
]
