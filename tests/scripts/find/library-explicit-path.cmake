cmake_minimum_required(VERSION 4.1)
set(CMAKE_LIBRARY_PATH "data/dir")
find_library(result NAMES needle NO_SYSTEM_ENVIRONMENT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
