cmake_minimum_required(VERSION 4.1)
find_file(result NAMES needle.h HINTS data/hint PATHS data/guess NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
