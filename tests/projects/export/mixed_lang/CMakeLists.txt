cmake_minimum_required(VERSION 3.20)
project(mixed_lang C CXX ASM)
add_library(mix STATIC mix.c mix.cpp mix.S)
export(TARGETS mix FILE "${CMAKE_BINARY_DIR}/mix-targets.cmake")
install(TARGETS mix EXPORT mx ARCHIVE DESTINATION lib)
install(EXPORT mx
  FILE mx-targets.cmake
  DESTINATION lib/mx)
