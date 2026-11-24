cmake_minimum_required(VERSION 4.1)
project(aix_install C)

# Shared library with AIX_SHARED_LIBRARY_ARCHIVE off (the archive layout is
# the CMP0182-NEW default).  Note: AIX_SHARED_LIBRARY_ARCHIVE=ON crashes the
# 4.1.0 reference binary under this GNU-on-AIX modelling, so the property is
# only exercised in the off state here.
add_library(sh SHARED lib.c)
set_target_properties(sh PROPERTIES AIX_SHARED_LIBRARY_ARCHIVE OFF)

# Shared library under the CMP0182 OLD default (unarchived).
cmake_policy(SET CMP0182 OLD)
add_library(shold SHARED lib.c)
cmake_policy(SET CMP0182 NEW)

add_library(m MODULE lib.c)
add_library(st STATIC lib.c)

# Executable with ENABLE_EXPORTS -> app.imp import archive artifact.
add_executable(app main.c)
set_target_properties(app PROPERTIES ENABLE_EXPORTS ON)
target_link_libraries(app PRIVATE sh)

install(TARGETS sh shold m st app EXPORT pk
  RUNTIME DESTINATION bin
  LIBRARY DESTINATION lib
  ARCHIVE DESTINATION arc)
install(EXPORT pk FILE pk-targets.cmake DESTINATION lib/cmake/pk NAMESPACE pk::)
export(EXPORT pk FILE ${CMAKE_BINARY_DIR}/pk-targets.cmake NAMESPACE pk::)
