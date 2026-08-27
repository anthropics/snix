# structured attrs, setting __json, which is unsupported
(builtins.derivation {

  name = "foo";
  system = ":";
  builder = ":";

  __structuredAttrs = true;
  foo = "bar";
  __json = "foo";
}).drvPath
