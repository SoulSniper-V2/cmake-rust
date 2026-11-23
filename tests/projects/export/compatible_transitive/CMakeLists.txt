cmake_minimum_required(VERSION 4.1)
project(compatible_transitive C)
add_library(base STATIC f.c)
set_target_properties(base PROPERTIES
  COMPATIBLE_INTERFACE_BOOL FLAG COMPATIBLE_INTERFACE_STRING STR
  INTERFACE_FLAG ON INTERFACE_STR one)
add_library(mid INTERFACE)
target_link_libraries(mid INTERFACE base)
add_library(top SHARED f.c)
target_link_libraries(top PUBLIC mid)
set_target_properties(top PROPERTIES INTERFACE_FLAG ON INTERFACE_STR one)
export(TARGETS top mid base FILE exp.cmake)
install(TARGETS top mid base EXPORT e)
install(EXPORT e DESTINATION lib/cmake/e)
