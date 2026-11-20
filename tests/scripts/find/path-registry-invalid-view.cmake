cmake_minimum_required(VERSION 4.1)
find_path(result NAMES needle.h REGISTRY_VIEW UNKNOWN)
message("picked=[${result}], cache=[$CACHE{result}]")
