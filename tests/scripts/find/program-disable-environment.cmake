cmake_minimum_required(VERSION 4.1)
set(ENV{CMAKE_PROGRAM_PATH} "data/dir")
find_program(result NAMES needle.tool HINTS data/hint NO_CMAKE_ENVIRONMENT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
