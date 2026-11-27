cmake_minimum_required(VERSION 3.20)
project(mkod C)
# Object rule prerequisites follow WriteObjectDependRules: flags.make, the
# source, then the source's OBJECT_DEPENDS (then PCH headers).
add_library(od STATIC a.c)
set_source_files_properties(a.c PROPERTIES OBJECT_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/inc/x.h)
target_precompile_headers(od PRIVATE inc/x.h)
