cmake_minimum_required(VERSION 4.1)
find_file(result NAMES needle.h NAMES_PER_DIR)
message("picked=[${result}], cache=[$CACHE{result}]")
