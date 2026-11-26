cmake_minimum_required(VERSION 3.20)
project(fsc C)
# A target's own FILE_SET HEADERS files are already in "Header Files" when
# its group copy is taken (sources are computed during Compute), so a
# regex group does not claim them. And CMAKE_INCLUDE_CURRENT_DIR entries
# that are SYSTEM for the target move to the end like any other.
set(CMAKE_INCLUDE_CURRENT_DIR ON)
source_group("Regex" REGULAR_EXPRESSION "x[.]h$")
add_executable(app main.c)
target_link_libraries(app PRIVATE sysinc lib)
add_library(lib STATIC l.c)
target_sources(lib PUBLIC FILE_SET HEADERS BASE_DIRS ${CMAKE_CURRENT_SOURCE_DIR}/inc FILES ${CMAKE_CURRENT_SOURCE_DIR}/inc/x.h)
add_library(sysinc STATIC s.c)
target_include_directories(sysinc SYSTEM PUBLIC ${CMAKE_CURRENT_BINARY_DIR})
