cmake_minimum_required(VERSION 3.20)
project(ci C)
# The codemodel's include paths come from GetIncludeDirectories with its
# defaults, which strip implicit directories even when the project asks
# for them explicitly.
add_library(lib STATIC l.c)
target_include_directories(lib SYSTEM PRIVATE /usr/include)
target_include_directories(lib PRIVATE /usr/include ${CMAKE_CURRENT_SOURCE_DIR})
