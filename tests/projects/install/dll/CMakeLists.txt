cmake_minimum_required(VERSION 4.1)
project(dll_install C)

# Shared library -> DLL plus import library.
add_library(dll SHARED lib.c)
set_target_properties(dll PROPERTIES VERSION 4.1.0 SOVERSION 4)

# Static library for the plain ARCHIVE group.
add_library(st STATIC lib.c)

# Executable exporting symbols -> import library artifact.
add_executable(app main.c)
set_target_properties(app PROPERTIES ENABLE_EXPORTS ON)
target_link_libraries(app PRIVATE dll)

install(TARGETS dll st app EXPORT pk
  RUNTIME DESTINATION bin
  LIBRARY DESTINATION lib
  ARCHIVE DESTINATION arc COMPONENT dev)
install(EXPORT pk FILE pk-targets.cmake DESTINATION lib/cmake/pk NAMESPACE pk::)
export(EXPORT pk FILE ${CMAKE_BINARY_DIR}/pk-targets.cmake NAMESPACE pk::)
