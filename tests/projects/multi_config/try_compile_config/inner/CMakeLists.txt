cmake_minimum_required(VERSION 4.1)
project(inner C)
message("types ${CMAKE_CONFIGURATION_TYPES} ${CMAKE_BUILD_TYPE} ${CMAKE_GENERATOR}")
add_executable(e e.c)
