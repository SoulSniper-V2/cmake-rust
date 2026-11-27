cmake_minimum_required(VERSION 4.1)
project(HeaderSet C)
add_library(lib STATIC src.c)
target_sources(lib PUBLIC FILE_SET HEADERS
  BASE_DIRS "${CMAKE_CURRENT_SOURCE_DIR}/headers" "${CMAKE_CURRENT_SOURCE_DIR}/headers2"
  FILES "$<1:headers/top.h>" "$<1:headers/sub/nested.h>" "$<1:headers2/other.h>")
install(TARGETS lib FILE_SET HEADERS)
