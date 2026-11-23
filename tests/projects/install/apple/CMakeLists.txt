cmake_minimum_required(VERSION 4.1)
project(apple_install C)

# Shared library with INSTALL_NAME_DIR and MACOSX_RPATH on.
add_library(named SHARED lib.c)
set_target_properties(named PROPERTIES
  INSTALL_NAME_DIR "@rpath/named"
  MACOSX_RPATH ON
  VERSION 3.2.1 SOVERSION 3)

# Shared library with MACOSX_RPATH off.
add_library(norpath SHARED lib.c)
set_target_properties(norpath PROPERTIES MACOSX_RPATH OFF)

# Static library for the ARCHIVE group.
add_library(st STATIC lib.c)

# Framework shared library.
add_library(fw SHARED lib.c)
set_target_properties(fw PROPERTIES
  FRAMEWORK TRUE
  FRAMEWORK_VERSION A
  MACOSX_FRAMEWORK_IDENTIFIER org.example.fw
  VERSION 2.0.0 SOVERSION 2)

# Bundled executable.
add_executable(tool MACOSX_BUNDLE main.c)
set_target_properties(tool PROPERTIES
  MACOSX_BUNDLE_GUI_IDENTIFIER org.example.tool
  MACOSX_BUNDLE_BUNDLE_VERSION 1.2)
target_link_libraries(tool PRIVATE named)

install(TARGETS named norpath st fw tool EXPORT pk
  RUNTIME DESTINATION bin
  LIBRARY DESTINATION lib COMPONENT libdev NAMELINK_COMPONENT namelink
  ARCHIVE DESTINATION arc
  FRAMEWORK DESTINATION Frameworks
  BUNDLE DESTINATION Applications)
install(EXPORT pk FILE pk-targets.cmake DESTINATION lib/cmake/pk NAMESPACE pk::)
export(EXPORT pk FILE ${CMAKE_BINARY_DIR}/pk-targets.cmake NAMESPACE pk::)
