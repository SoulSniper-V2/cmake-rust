cmake_minimum_required(VERSION 4.1)
find_library(result NAMES needle REQUIRED OPTIONAL NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
