cmake_minimum_required(VERSION 4.1)
set(ENV{LIB} "${CMAKE_CURRENT_SOURCE_DIR}/data/dir")
find_library(result NAMES needle)
message("picked=[${result}], cache=[$CACHE{result}]")
