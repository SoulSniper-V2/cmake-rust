cmake_minimum_required(VERSION 4.1)
set(ENV{PATH} "${CMAKE_CURRENT_SOURCE_DIR}/data/dir")
find_program(result NAMES needle.tool)
message("picked=[${result}], cache=[$CACHE{result}]")
