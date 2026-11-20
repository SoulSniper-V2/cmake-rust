cmake_minimum_required(VERSION 4.1)
find_file(result NAMES needle.h REQUIRED OPTIONAL NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
