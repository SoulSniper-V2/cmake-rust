cmake_minimum_required(VERSION 3.20)
project(install_dest C)
add_library(d STATIC d.c)
install(TARGETS d EXPORT pd
  ARCHIVE DESTINATION lib
  INCLUDES DESTINATION include/d)
install(EXPORT pd
  FILE pd-targets.cmake
  DESTINATION lib/cmake/proj)
