cmake_minimum_required(VERSION 4.1)
find_path(result needle.h VALIDATOR missing_function NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
