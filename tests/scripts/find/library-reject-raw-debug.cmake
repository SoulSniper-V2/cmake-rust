cmake_minimum_required(VERSION 4.1)
set(CMAKE_FIND_DEBUG_MODE ON)
find_library(ITEM NAMES libmissing.so PATHS data/lib NO_DEFAULT_PATH)
message("library=${ITEM}")
