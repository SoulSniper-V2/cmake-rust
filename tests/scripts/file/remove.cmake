file(WRITE a/b/file data)
file(REMOVE a/b a/b/file absent)
file(GLOB_RECURSE out LIST_DIRECTORIES TRUE RELATIVE "${CMAKE_CURRENT_BINARY_DIR}" "a/*")
message("[${out}]")
