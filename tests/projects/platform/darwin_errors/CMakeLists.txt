cmake_minimum_required(VERSION 3.29)
project(DarwinErrors C)
add_executable(app main.c)
set_target_properties(app PROPERTIES MSVC_RUNTIME_LIBRARY Bogus)
