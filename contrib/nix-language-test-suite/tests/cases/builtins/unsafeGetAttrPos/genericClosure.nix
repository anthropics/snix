let
  closure = builtins.genericClosure {
    startSet = [{key = 5;}];
    operator = item: [
      {
        key =
          if (item.key / 2) * 2 == item.key
          then item.key / 2
          else 3 * item.key + 1;
      }
    ];
  };
in
[
  # 0th element is `key = 5` which is a part of `startSet, so
  # its position should point to `startSet` line
  (builtins.unsafeGetAttrPos "key" (builtins.elemAt closure 0))

  # 1st element is generated, so its position points to the generation function
  (builtins.unsafeGetAttrPos "key" (builtins.elemAt closure 1))
]
