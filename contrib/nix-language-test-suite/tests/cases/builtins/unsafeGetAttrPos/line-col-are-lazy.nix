let
  attrset = {a = 1;};
  pos = builtins.unsafeGetAttrPos "a" attrset;
in
  pos
