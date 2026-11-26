cmake_minimum_required(VERSION 3.20)
project(top C CXX ASM)
# Headers next to C-like sources are found with cmake's fixed header
# extension list; a sub-project's files are listed relative to the top.
add_library(lib STATIC a.cpp b.cxx c.c d.cc e.s hdr.h)
set_source_files_properties(hdr.h PROPERTIES LANGUAGE C)
add_subdirectory(sub)
# A sub-project whose sources live outside the top source tree.
file(WRITE ${CMAKE_BINARY_DIR}/ext-src/CMakeLists.txt "project(extproj C)\nadd_library(ext STATIC e.c)\n")
file(WRITE ${CMAKE_BINARY_DIR}/ext-src/e.c "int e(void){return 0;}\n")
file(WRITE ${CMAKE_BINARY_DIR}/ext-src/e.h "\n")
add_subdirectory(${CMAKE_BINARY_DIR}/ext-src ${CMAKE_BINARY_DIR}/ext-build)
