cmake_minimum_required(VERSION 3.20)
project(opts C CXX)
add_library(iface INTERFACE)
target_precompile_headers(iface INTERFACE <string> "${CMAKE_CURRENT_SOURCE_DIR}/common.h")
add_library(lib STATIC a.cxx b.cxx c.c skip.cxx)
target_link_libraries(lib PUBLIC iface)
set_source_files_properties(skip.cxx PROPERTIES SKIP_PRECOMPILE_HEADERS ON SKIP_UNITY_BUILD_INCLUSION ON)
set_target_properties(lib PROPERTIES UNITY_BUILD ON UNITY_BUILD_BATCH_SIZE 1
  UNITY_BUILD_UNIQUE_ID MY_UNITY_ID
  UNITY_BUILD_CODE_BEFORE_INCLUDE "// before"
  UNITY_BUILD_CODE_AFTER_INCLUDE "// after")
add_executable(grouped main.cxx g1.cxx g2.cxx g3.cxx)
set_target_properties(grouped PROPERTIES UNITY_BUILD ON UNITY_BUILD_MODE GROUP UNITY_BUILD_RELOCATABLE ON)
set_source_files_properties(g1.cxx g2.cxx PROPERTIES UNITY_GROUP first)
set_source_files_properties(g3.cxx PROPERTIES UNITY_GROUP second)
target_link_libraries(grouped lib)
add_executable(nopch main.cxx)
target_link_libraries(nopch iface)
set_target_properties(nopch PROPERTIES DISABLE_PRECOMPILE_HEADERS ON)
