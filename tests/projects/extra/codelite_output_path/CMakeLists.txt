cmake_minimum_required(VERSION 3.20)
project(outp C)
# cmSystemTools::RelativePath collapses "." components and spells a pure
# ascent with a trailing "../".
set(EXECUTABLE_OUTPUT_PATH ${CMAKE_BINARY_DIR}/./bin)
add_executable(app main.c)
add_subdirectory(sub)
