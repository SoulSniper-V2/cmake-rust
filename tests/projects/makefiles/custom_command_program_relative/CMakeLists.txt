cmake_minimum_required(VERSION 3.20)
project(mkcc C)
# cmLocalUnixMakefileGenerator3::AppendCustomCommand: with no
# WORKING_DIRECTORY the program path is made relative to the current binary
# directory (with "./" when it lands in it); with one it stays as given.
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/bin)
add_subdirectory(tool)
add_subdirectory(use)
