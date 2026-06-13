[
  # value's position returned
  (builtins.unsafeGetAttrPos "foo" (builtins.listToAttrs [
    {
      name = "foo";
      value = 1;
    }
  ]))

  # the order of fields does not matter, still value's position
  (builtins.unsafeGetAttrPos "foo" (builtins.listToAttrs [
    {
      value = 1;
      name = "foo";
    }
  ]))
]
