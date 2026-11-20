cmake_minimum_required(VERSION 4.1)
find_program(result needle.tool VALIDATOR missing_function NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
