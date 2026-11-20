cmake_minimum_required(VERSION 4.1)
set(CMAKE_FIND_ROOT_PATH "${CMAKE_CURRENT_SOURCE_DIR}/data/root")
find_program(result NAMES needle.tool PATHS /find-test-root NO_DEFAULT_PATH NO_CMAKE_FIND_ROOT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
