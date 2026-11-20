cmake_minimum_required(VERSION 4.1)
set(ENV{INCLUDE} "${CMAKE_CURRENT_SOURCE_DIR}/data/dir")
find_file(result NAMES needle.h)
message("picked=[${result}], cache=[$CACHE{result}]")
