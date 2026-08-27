let
  base = {
    name = "foo";
    system = ":";
    builder = ":";
  };
in [
  # structured attrs set to false will render an empty string inside env
  (builtins.derivation (base
    // {
      __structuredAttrs = false;
      foo = "bar";
    })).drvPath

  (builtins.derivation (base
    // {
      __structuredAttrs = false;
      foo = "bar";
    })).outPath

  # simple structured attrs
  (builtins.derivation (base
    // {
      __structuredAttrs = true;
      foo = "bar";
    })).drvPath

  (builtins.derivation (base
    // {
      __structuredAttrs = true;
      foo = "bar";
    })).outPath

  # structured attrs with outputsCheck
  (builtins.derivation (base
    // {
      __structuredAttrs = true;
      foo = "bar";
      outputChecks = {
        out = {
          maxClosureSize = 256 * 1024 * 1024;
          disallowedRequisites = ["dev"];
        };
      };
    })).drvPath

  (builtins.derivation (base
    // {
      __structuredAttrs = true;
      foo = "bar";
      outputChecks = {
        out = {
          maxClosureSize = 256 * 1024 * 1024;
          disallowedRequisites = ["dev"];
        };
      };
    })).outPath

  # structured attrs and __ignoreNulls.
  # ignoreNulls is inactive (so foo ends up in __json, yet __ignoreNulls itself is not present.
  (builtins.derivation (base
    // {
      __ignoreNulls = false;
      foo = null;
      __structuredAttrs = true;
    })).drvPath

  # structured attrs, setting outputs.
  (builtins.derivation {
    name = "test";
    system = "aarch64-linux";
    builder = "/bin/sh";
    __structuredAttrs = true;
    outputs = ["out"];
  }).drvPath
]
