cmake_minimum_required(VERSION 4.1)
project(ReplaceEscape NONE)
add_custom_target(replace
  COMMAND "${CMAKE_COMMAND}" -E echo [[$<LIST:TRANSFORM,a,REPLACE,a,\é>]]
  VERBATIM)
