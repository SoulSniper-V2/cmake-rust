cmake_minimum_required(VERSION 4.1)
set(ENV{CMAKE_INCLUDE_PATH} "data/missing:data/dir")
find_file(result NAMES needle.h NO_SYSTEM_ENVIRONMENT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
