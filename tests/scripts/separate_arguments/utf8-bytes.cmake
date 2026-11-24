# Non-ASCII bytes pass through each parser unchanged; a vertical tab is
# C-locale whitespace.
separate_arguments(out UNIX_COMMAND "a é \"ü x\" qr")
message("${out}")
separate_arguments(o2 WINDOWS_COMMAND "ñ \"b ö\" cd")
message("${o2}")
separate_arguments(o3 NATIVE_COMMAND PROGRAM SEPARATE_ARGS "/usr/bin/ñ x é")
message("${o3}")
