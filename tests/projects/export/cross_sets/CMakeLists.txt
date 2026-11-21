cmake_minimum_required(VERSION 3.20)
project(cross_sets C)
add_library(dep STATIC dep.c)
add_library(mid STATIC mid.c)
target_link_libraries(mid PRIVATE dep)
add_library(top STATIC top.c)
target_link_libraries(top PRIVATE mid)
install(TARGETS dep EXPORT dep ARCHIVE DESTINATION lib)
install(TARGETS mid top EXPORT app ARCHIVE DESTINATION lib)
export(EXPORT dep
  FILE "${CMAKE_BINARY_DIR}/dep-targets.cmake"
  NAMESPACE dep::)
export(EXPORT app
  FILE "${CMAKE_BINARY_DIR}/app-targets.cmake"
  NAMESPACE app::)
install(EXPORT dep
  NAMESPACE dep::
  FILE dep-targets.cmake
  DESTINATION lib/cmake)
install(EXPORT app
  NAMESPACE app::
  FILE app-targets.cmake
  DESTINATION lib/cmake)
