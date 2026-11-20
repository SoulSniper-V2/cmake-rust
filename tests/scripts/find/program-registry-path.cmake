cmake_minimum_required(VERSION 4.1)
find_program(result NAMES needle.tool PATHS "[HKCU/Software/FindTest;InstallDir]" data/dir NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
