separate_arguments(v WINDOWS_COMMAND [=[a \\"q b]=])
message(STATUS "v=${v}")
separate_arguments(w WINDOWS_COMMAND [=["a b" c]=])
message(STATUS "w=${w}")
