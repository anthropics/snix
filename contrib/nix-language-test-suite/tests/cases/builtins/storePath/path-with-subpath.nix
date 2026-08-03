let
  path = builtins.unsafeDiscardStringContext "${./dummy}";
in {
  plain = builtins.storePath path;
  withSubPath = builtins.storePath (path + "/.keep");
}
