cmake_minimum_required(VERSION 4.1)
set(CMAKE_IGNORE_PATH "${CMAKE_CURRENT_SOURCE_DIR}/data/dir/")
find_path(result NAMES needle.h PATHS data/dir data/fallback NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
