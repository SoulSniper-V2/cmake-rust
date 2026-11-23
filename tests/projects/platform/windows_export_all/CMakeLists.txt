cmake_minimum_required(VERSION 3.31)
project(windows_export_all C)
add_library(dll SHARED lib.c)
set_target_properties(dll PROPERTIES WINDOWS_EXPORT_ALL_SYMBOLS ON)
