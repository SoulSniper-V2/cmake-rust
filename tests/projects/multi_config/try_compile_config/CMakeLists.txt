cmake_minimum_required(VERSION 4.1)
project(try_compile_config C)
file(WRITE ${CMAKE_CURRENT_BINARY_DIR}/cfg.c [=[
#include <stdio.h>
int main(void) { printf("%s", CMAKE_INTDIR); return 0; }
]=])
add_library(imp STATIC IMPORTED)
set_target_properties(imp PROPERTIES
  IMPORTED_CONFIGURATIONS "DEBUG;RELEASE"
  IMPORTED_LOCATION_DEBUG ${CMAKE_CURRENT_BINARY_DIR}/d.a
  IMPORTED_LOCATION_RELEASE ${CMAKE_CURRENT_BINARY_DIR}/r.a)
foreach(config IN ITEMS "" Release RelWithDebInfo Missing)
  set(CMAKE_TRY_COMPILE_CONFIGURATION "${config}")
  try_run(ran compiled SOURCES ${CMAKE_CURRENT_BINARY_DIR}/cfg.c
    COPY_FILE ${CMAKE_CURRENT_BINARY_DIR}/copied-${config}
    COPY_FILE_ERROR copy_error
    RUN_OUTPUT_VARIABLE out NO_CACHE)
  message(STATUS "[${config}] ${compiled} ${ran} ${out} ${copy_error}")
  try_compile(linked SOURCES ${CMAKE_CURRENT_BINARY_DIR}/cfg.c
    LINK_LIBRARIES imp OUTPUT_VARIABLE log NO_CACHE)
  string(REGEX MATCH "[dr]\\.a" which "${log}")
  message(STATUS "[${config}] ${linked} ${which}")
  try_compile(project PROJECT inner SOURCE_DIR ${CMAKE_CURRENT_SOURCE_DIR}/inner
    OUTPUT_VARIABLE log NO_CACHE)
  string(REGEX MATCH "build-[A-Za-z]*\\.ninja" which "${log}")
  message(STATUS "[${config}] ${project} ${which}")
endforeach()
