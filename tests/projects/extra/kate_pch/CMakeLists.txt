cmake_minimum_required(VERSION 3.20)
project(KatePch C)
set(CMAKE_KATE_FILES_MODE LIST)
# The generated cmake_pch.h is a source of the directory
# (cmLocalGenerator::AddPchDependencies calls GetOrCreateSource on it).
add_library(lib STATIC lib.c)
target_precompile_headers(lib PRIVATE pch.h)
