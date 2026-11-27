cmake_minimum_required(VERSION 4.1)
project(IsSubdirectory NONE)
set(CMAKE_DISABLE_SOURCE_CHANGES ON)
get_filename_component(parent "${CMAKE_CURRENT_SOURCE_DIR}" DIRECTORY)
get_filename_component(leaf "${CMAKE_CURRENT_SOURCE_DIR}" NAME)
execute_process(COMMAND "${CMAKE_COMMAND}" -E echo sentinel
  OUTPUT_FILE "${parent}//${leaf}/out.txt")
