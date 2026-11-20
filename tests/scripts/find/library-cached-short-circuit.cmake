set(result "already-defined")
find_library(result NAMES needle VALIDATOR missing_function)
message("picked=[${result}], cache=[$CACHE{result}]")
