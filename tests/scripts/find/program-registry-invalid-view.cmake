cmake_minimum_required(VERSION 4.1)
find_program(result NAMES needle.tool REGISTRY_VIEW UNKNOWN)
message("picked=[${result}], cache=[$CACHE{result}]")
