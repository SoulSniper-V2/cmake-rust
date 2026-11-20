cmake_minimum_required(VERSION 4.1)
find_library(result NAMES needle REGISTRY_VIEW UNKNOWN)
message("picked=[${result}], cache=[$CACHE{result}]")
