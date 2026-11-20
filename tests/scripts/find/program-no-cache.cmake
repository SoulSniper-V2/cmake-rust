cmake_minimum_required(VERSION 4.1)
find_program(result NAMES needle.tool PATHS data/dir NO_DEFAULT_PATH NO_CACHE)
message("picked=[${result}], cache=[$CACHE{result}]")
