cmake_minimum_required(VERSION 4.1)
set(ENV{CMAKE_INCLUDE_PATH} "data/dir")
find_file(result NAMES needle.h HINTS data/hint NO_CMAKE_ENVIRONMENT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
