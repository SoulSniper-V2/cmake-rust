cmake_minimum_required(VERSION 4.1)
find_library(result NAMES libtrial.so.2 HINTS data/dir NO_DEFAULT_PATH)
message("${result}")
