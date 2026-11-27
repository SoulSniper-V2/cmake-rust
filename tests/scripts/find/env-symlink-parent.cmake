cmake_minimum_required(VERSION 4.1)
file(MAKE_DIRECTORY real/sub)
file(WRITE real/needle.h "real")
file(WRITE needle.h "cwd")
file(CREATE_LINK "${CMAKE_CURRENT_BINARY_DIR}/real/sub" link SYMBOLIC)
set(ENV{CMAKE_INCLUDE_PATH} "${CMAKE_CURRENT_BINARY_DIR}/link/..")
find_file(header needle.h NO_CMAKE_PATH NO_SYSTEM_ENVIRONMENT_PATH NO_CMAKE_SYSTEM_PATH)
message("${header}")

# A CMake variable uses CollapseFullPath instead of the on-disk resolver.
set(CMAKE_INCLUDE_PATH "${CMAKE_CURRENT_BINARY_DIR}/link/..")
find_file(var_header needle.h NO_CMAKE_ENVIRONMENT_PATH NO_SYSTEM_ENVIRONMENT_PATH NO_CMAKE_SYSTEM_PATH)
message("${var_header}")
