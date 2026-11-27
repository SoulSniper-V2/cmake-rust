cmake_minimum_required(VERSION 3.21)
project(FileGenerateInput NONE)
file(GENERATE OUTPUT generated-$<CONFIG>.txt INPUT ${CMAKE_CURRENT_SOURCE_DIR}/input.txt)
