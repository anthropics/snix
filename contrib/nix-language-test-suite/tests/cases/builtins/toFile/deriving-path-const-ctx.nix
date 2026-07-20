# A (`{ path = true; }`) context element is allowed.
builtins.toFile "hello" "${./hello.txt}"
