# Test that builtins.toXML correctly preserves parameter names for nested functions.
# The outermost function parameter should be shown in the XML output.
builtins.toXML (outer: inner: outer + inner)
