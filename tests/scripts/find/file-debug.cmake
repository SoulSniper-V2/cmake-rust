cmake_minimum_required(VERSION 4.1)
set(CMAKE_FIND_DEBUG_MODE TRUE)
find_file(result NAMES first second PATHS data/dir NO_DEFAULT_PATH)
message("${result}")
