cmake_minimum_required(VERSION 4.1)
set(ENV{CMAKE_LIBRARY_PATH} "data/dir")
find_library(result NAMES needle HINTS data/hint NO_CMAKE_ENVIRONMENT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
