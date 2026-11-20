cmake_minimum_required(VERSION 4.1)
find_library(result NAMES needle PATHS "[HKCU/Software/FindTest;InstallDir]" data/dir NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
