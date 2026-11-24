cmake_minimum_required(VERSION 3.20)
project(p NONE)
enable_testing()
add_subdirectory(a)
# A binary directory outside the top binary directory stays absolute.
add_subdirectory(c "${CMAKE_BINARY_DIR}/../outside-bin")
