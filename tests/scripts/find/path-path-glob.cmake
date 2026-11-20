cmake_minimum_required(VERSION 4.1)
find_path(result NAMES needle.h PATHS "${CMAKE_CURRENT_SOURCE_DIR}/data/*/leaf" NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
