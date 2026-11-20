cmake_minimum_required(VERSION 4.1)
find_library(result NAMES needle PATHS data/dir PATH_SUFFIXES /sub/ NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
