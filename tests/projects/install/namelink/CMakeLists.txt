cmake_minimum_required(VERSION 4.1)
project(elf_install C)

# Versioned shared library: libv.so.5.6.7 + libv.so.5 soname + libv.so
# namelink, split into "runtime" and "dev" components.
add_library(v SHARED lib.c)
set_target_properties(v PROPERTIES VERSION 5.6.7 SOVERSION 5)

# NAMELINK_SKIP drops the unversioned link; NAMELINK_ONLY installs only it.
add_library(skip SHARED lib.c)
set_target_properties(skip PROPERTIES VERSION 1.0.0 SOVERSION 1)
add_library(only SHARED lib.c)
set_target_properties(only PROPERTIES VERSION 2.3.4 SOVERSION 2)

add_executable(app main.c)
target_link_libraries(app PRIVATE v skip only)

install(TARGETS v EXPORT pk
  LIBRARY DESTINATION lib COMPONENT runtime NAMELINK_COMPONENT dev
  RUNTIME DESTINATION bin
  ARCHIVE DESTINATION arc)
install(TARGETS skip LIBRARY DESTINATION lib NAMELINK_SKIP)
install(TARGETS only LIBRARY DESTINATION lib NAMELINK_ONLY)

# A named runtime dependency set plus an implicit RUNTIME_DEPENDENCIES group.
install(TARGETS app RUNTIME_DEPENDENCY_SET appdeps
  RUNTIME DESTINATION bin)
install(RUNTIME_DEPENDENCY_SET appdeps
  PRE_EXCLUDE_REGEXES "^libc\\."
  LIBRARY DESTINATION lib/deps COMPONENT deps)
install(TARGETS app RUNTIME_DEPENDENCIES DIRECTORIES /opt/dep
  RUNTIME DESTINATION bin2)

install(EXPORT pk FILE pk-targets.cmake DESTINATION lib/cmake/pk NAMESPACE pk::)
export(EXPORT pk FILE ${CMAKE_BINARY_DIR}/pk-targets.cmake NAMESPACE pk::)
