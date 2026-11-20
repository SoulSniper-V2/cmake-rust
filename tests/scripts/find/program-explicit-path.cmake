cmake_minimum_required(VERSION 4.1)
set(CMAKE_PROGRAM_PATH "data/dir")
find_program(result NAMES needle.tool NO_SYSTEM_ENVIRONMENT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
