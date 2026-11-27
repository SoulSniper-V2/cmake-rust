cmake_minimum_required(VERSION 4.1)
project(compiler_include_paths C)

set(ENV{CPATH} "~/override/include")
set(CMAKE_C_IMPLICIT_INCLUDE_DIRECTORIES
  "${CMAKE_CURRENT_SOURCE_DIR}/sdk/usr/include"
  "$ENV{HOME}/override/include")
set(CMAKE_C_STANDARD_INCLUDE_DIRECTORIES "//server/mount/include" "~/sdk/includes")

add_library(example STATIC example.c)
target_include_directories(example PRIVATE /usr/include "$ENV{HOME}/override/include")
