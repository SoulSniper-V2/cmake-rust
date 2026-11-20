cmake_minimum_required(VERSION 4.1)
set(CMAKE_FIND_LIBRARY_CUSTOM_LIB_SUFFIX alt)
find_library(result NAMES trial HINTS data/lib NO_DEFAULT_PATH)
message("${result}")
