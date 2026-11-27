cmake_minimum_required(VERSION 3.20)
project(mkdo C)
# Makefile2 directory rules follow ComputeDirectoryTargets' std::map on the
# binary directory ("a" before "a-b" regardless of add order), and each
# CMakeDirectoryInformation.cmake names that directory's own relative-path
# tops (ComputeRelativePathTopSource): "outside" is not under the top source.
add_subdirectory(a-b)
add_subdirectory(a)
set(outside ${CMAKE_BINARY_DIR}/../outside)
file(WRITE ${outside}/CMakeLists.txt "add_library(o STATIC o.c)\n")
file(WRITE ${outside}/o.c "int o(void) { return 3; }\n")
add_subdirectory(${outside} outside)
