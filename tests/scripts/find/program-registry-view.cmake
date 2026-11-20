cmake_minimum_required(VERSION 4.1)
find_program(result NAMES needle.tool REGISTRY_VIEW HOST PATHS data/dir NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
