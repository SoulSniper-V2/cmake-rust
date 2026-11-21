cmake_minimum_required(VERSION 3.20)
project(genex_props C)
add_library(other STATIC other.c)
add_library(g STATIC g.c)
target_include_directories(g INTERFACE
  $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/inc>
  $<INSTALL_INTERFACE:include/g>)
target_compile_definitions(g INTERFACE $<$<CONFIG:Debug>:G_DEBUG>)
target_link_libraries(g PRIVATE other)
set_target_properties(g PROPERTIES EXPORT_NAME grex)
install(TARGETS g other EXPORT gs
  ARCHIVE DESTINATION lib
  INCLUDES DESTINATION include/g)
install(EXPORT gs
  NAMESPACE g_
  FILE gs-targets.cmake
  DESTINATION lib/cmake)
export(EXPORT gs
  NAMESPACE g_
  FILE "${CMAKE_BINARY_DIR}/gs-targets.cmake")
