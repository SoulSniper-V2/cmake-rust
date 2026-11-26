cmake_minimum_required(VERSION 3.20)
project(p C)
# Directory-scoped imported executables as a custom command's argv0: visible
# in the defining directory and in subdirectories created after it.
add_executable(early IMPORTED)
set_property(TARGET early PROPERTY IMPORTED_LOCATION /usr/bin/true)
add_subdirectory(sib)
add_subdirectory(sub)
add_executable(late IMPORTED)
set_property(TARGET late PROPERTY IMPORTED_LOCATION /usr/bin/false)
add_custom_command(OUTPUT top.h COMMAND early top.h COMMAND late top.h COMMAND sibtool top.h)
add_custom_target(top ALL DEPENDS top.h)
