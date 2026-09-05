# Test that builtins.toXML correctly preserves function parameter names
# instead of using hardcoded "x" for simple function parameters.
builtins.toXML (myParam: myParam)
