file(MAKE_DIRECTORY a b/c RESULT result)
file(GLOB out RELATIVE "${CMAKE_CURRENT_BINARY_DIR}" "*")
message("${result}")
message("[${out}]")
