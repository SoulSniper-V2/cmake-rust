cmake_minimum_required(VERSION 4.1)
find_file(result NAMES absent NO_DEFAULT_PATH NO_CACHE)
find_file(result NAMES needle.h PATHS data/dir NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
