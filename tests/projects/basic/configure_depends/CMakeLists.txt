cmake_minimum_required(VERSION 3.20)
project(ConfigureDepends NONE)
set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS "input.txt" "${CMAKE_CURRENT_SOURCE_DIR}/abs.txt")
add_subdirectory(sub)
