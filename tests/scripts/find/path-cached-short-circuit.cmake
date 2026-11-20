set(result "already-defined")
find_path(result NAMES needle.h VALIDATOR missing_function)
message("picked=[${result}], cache=[$CACHE{result}]")
