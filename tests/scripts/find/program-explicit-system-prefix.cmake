cmake_minimum_required(VERSION 4.1)
set(CMAKE_SYSTEM_PREFIX_PATH "data/prefix")
find_program(result NAMES needle.tool NO_SYSTEM_ENVIRONMENT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
