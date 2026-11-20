cmake_minimum_required(VERSION 4.1)
set(ENV{CMAKE_PROGRAM_PATH} "data/missing:data/dir")
find_program(result NAMES needle.tool NO_SYSTEM_ENVIRONMENT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
