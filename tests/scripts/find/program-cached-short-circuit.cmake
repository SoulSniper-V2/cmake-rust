set(result "already-defined")
find_program(result NAMES needle.tool VALIDATOR missing_function)
message("picked=[${result}], cache=[$CACHE{result}]")
