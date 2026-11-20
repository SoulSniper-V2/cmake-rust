cmake_minimum_required(VERSION 4.1)
set(ENV{CMAKE_LIBRARY_PATH} "data/missing:data/dir")
find_library(result NAMES needle NO_SYSTEM_ENVIRONMENT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
