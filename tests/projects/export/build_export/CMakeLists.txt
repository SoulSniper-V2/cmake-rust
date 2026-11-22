cmake_minimum_required(VERSION 3.20)
project(build_export C)
add_library(base STATIC base.c)
add_library(s STATIC s.c)
target_link_libraries(s PRIVATE base)
add_library(sh SHARED sh.c)
add_library(o OBJECT o.c)
add_library(i INTERFACE)
add_executable(e e.c)
set_target_properties(e PROPERTIES ENABLE_EXPORTS ON)
export(TARGETS base s sh o i e
  FILE "${CMAKE_BINARY_DIR}/exp.cmake"
  NAMESPACE bx_)
