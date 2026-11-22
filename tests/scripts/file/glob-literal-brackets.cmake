file(WRITE "[name].txt" bracket)
file(WRITE "other.txt" other)
file(GLOB paths RELATIVE "${CMAKE_CURRENT_BINARY_DIR}" "[[]name[]].txt")
message("[${paths}]")
