cmake_minimum_required(VERSION 4.1)
set(CMAKE_IGNORE_PATH "${CMAKE_CURRENT_SOURCE_DIR}/data/dir/")
find_library(result NAMES needle PATHS data/dir data/fallback NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
