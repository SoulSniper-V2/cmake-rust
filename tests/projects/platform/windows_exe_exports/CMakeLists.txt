cmake_minimum_required(VERSION 3.31)
project(windows_exe_exports C)
add_executable(app main.c)
set_target_properties(app PROPERTIES ENABLE_EXPORTS ON)
