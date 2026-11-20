cmake_minimum_required(VERSION 4.1)
find_file(result NAMES needle.h PATHS data/dir NO_DEFAULT_PATH NO_CACHE)
message("picked=[${result}], cache=[$CACHE{result}]")
