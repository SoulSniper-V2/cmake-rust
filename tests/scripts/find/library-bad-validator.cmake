cmake_minimum_required(VERSION 4.1)
find_library(result needle VALIDATOR missing_function NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
