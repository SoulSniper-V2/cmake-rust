cmake_minimum_required(VERSION 4.1)
project(ListSort NONE)
foreach(i RANGE 0 17)
  list(APPEND paths "dir${i}/item")
endforeach()
add_custom_target(sorted
  COMMAND "${CMAKE_COMMAND}" -E echo "$<LIST:SORT,${paths},COMPARE:FILE_BASENAME>"
  VERBATIM)
