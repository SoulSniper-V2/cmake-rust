cmake_minimum_required(VERSION 4.1)
set(CMAKE_FIND_REQUIRED ON)
find_file(result NAMES needle.h OPTIONAL NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
