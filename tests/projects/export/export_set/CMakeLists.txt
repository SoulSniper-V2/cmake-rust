cmake_minimum_required(VERSION 3.20)
project(export_set C)
add_library(a STATIC a.c)
add_library(b SHARED b.c)
target_link_libraries(a PRIVATE b)
install(TARGETS a b EXPORT es
  ARCHIVE DESTINATION lib
  LIBRARY DESTINATION lib
  RUNTIME DESTINATION bin)
export(EXPORT es
  FILE "${CMAKE_BINARY_DIR}/es-targets.cmake"
  NAMESPACE es_)
