set(result "already-defined")
find_file(result NAMES needle.h VALIDATOR missing_function)
message("picked=[${result}], cache=[$CACHE{result}]")
