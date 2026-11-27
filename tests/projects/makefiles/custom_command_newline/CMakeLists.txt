cmake_minimum_required(VERSION 4.1)
project(Newline NONE)
add_custom_target(echo_lines
  COMMAND "${CMAKE_COMMAND}" -E echo "first\nsecond"
  VERBATIM)
