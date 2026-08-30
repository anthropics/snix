let
  foo = "foo";
in
{
  notfirst_abs = /bar/${foo};
  normalized = ./path/to/../${foo};
  normalized_abs = /path/to/../${foo};
  normalized_home = ~/path/to/../${foo};
}
