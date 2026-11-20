get_property(e DIRECTORY PROPERTY EXCLUDE_FROM_ALL)
get_property(s DIRECTORY PROPERTY SYSTEM)
message("in sub3 ${CMAKE_CURRENT_BINARY_DIR} e=[${e}] s=[${s}]")
