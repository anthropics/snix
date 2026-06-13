let
  attr-path-11 = {foo.bar = 1;};
  attr-path-12 = {foo.bar.baz = 1;};

  attr-path-21 = {foo = {bar = 1;};};
  attr-path-22 = {foo = {bar.baz = 1;};};

  attr-path-3 = {};
  attr-path-3.foo = 1;
in [
  # For attr paths, the first segment is returned e.g
  # for `foo.bar` it must be the position of `foo`
  (builtins.unsafeGetAttrPos "bar" attr-path-11.foo)
  # `baz`'s positions refers to `foo` too
  (builtins.unsafeGetAttrPos "baz" attr-path-12.foo.bar)

  # But if `bar` is declared in an explicitly nested attset,
  # its position is returned
  (builtins.unsafeGetAttrPos "bar" attr-path-21.foo)
  (builtins.unsafeGetAttrPos "baz" attr-path-22.foo.bar)

  (builtins.unsafeGetAttrPos "foo" attr-path-3)
]
