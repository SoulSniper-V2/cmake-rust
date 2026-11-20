file(WRITE tree/c/x.log x)
file(WRITE tree/b/a.txt y)
file(GLOB_RECURSE out RELATIVE "${CMAKE_CURRENT_BINARY_DIR}" LIST_DIRECTORIES ON "tree/*.txt")
message("[${out}]")
