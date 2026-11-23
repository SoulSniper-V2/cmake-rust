cmake_minimum_required(VERSION 4.1)
project(rd C)
add_library(dep SHARED b.c)
add_library(b SHARED b.c)
target_link_libraries(b PRIVATE $<$<CONFIG:Debug>:dep>)
add_library(m MODULE b.c)
add_executable(tool main.c)
target_link_libraries(tool PRIVATE b)
set_property(TARGET b PROPERTY DEBUG_POSTFIX d)
install(TARGETS tool b m RUNTIME_DEPENDENCIES
  DIRECTORIES /opt/$<CONFIG> PRE_EXCLUDE_REGEXES "^libc\\." $<$<CONFIG:Debug>:dbg>
  POST_INCLUDE_FILES "/a/b" RUNTIME DESTINATION bin/$<CONFIG> LIBRARY DESTINATION lib/$<CONFIG>)
install(TARGETS tool RUNTIME_DEPENDENCY_SET set CONFIGURATIONS Release Debug)
install(RUNTIME_DEPENDENCY_SET set LIBRARY DESTINATION lib2/$<CONFIG> COMPONENT rt
  CONFIGURATIONS Debug)
add_library(imp SHARED IMPORTED)
set_target_properties(imp PROPERTIES IMPORTED_LOCATION /x/libimp.so IMPORTED_LOCATION_DEBUG /x/libimpd.so)
install(IMPORTED_RUNTIME_ARTIFACTS imp LIBRARY DESTINATION imp/$<CONFIG>)
