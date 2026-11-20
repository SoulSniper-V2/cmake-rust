cmake_minimum_required(VERSION 4.1)
find_library(first NAMES first second PATHS data/one data/two NO_DEFAULT_PATH)
find_library(second NAMES first second PATHS data/one data/two NO_DEFAULT_PATH NAMES_PER_DIR)
message("${first}|${second}")
