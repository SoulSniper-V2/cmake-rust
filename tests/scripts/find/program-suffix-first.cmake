cmake_minimum_required(VERSION 4.1)
find_program(result NAMES needle.tool PATHS data/dir PATH_SUFFIXES /sub/ NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
