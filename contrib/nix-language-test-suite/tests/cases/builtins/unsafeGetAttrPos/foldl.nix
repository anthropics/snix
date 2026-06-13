let
  # lambda with update operator inside preserves position
  fold = builtins.foldl' (acc: x: acc // {${x} = true;}) {} ["a" "b" "c"];

  # but a lambda without an update does not
  fold-no-update = builtins.foldl' (acc: x: {${x} = true;}) {} ["a" "b" "c"];
in
[
  (builtins.unsafeGetAttrPos "a" fold)
  (builtins.unsafeGetAttrPos "a" fold-no-update)
]
