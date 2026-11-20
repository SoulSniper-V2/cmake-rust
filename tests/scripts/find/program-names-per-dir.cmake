cmake_minimum_required(VERSION 4.1)
find_program(first NAMES first.tool second.tool PATHS data/one data/two NO_DEFAULT_PATH)
find_program(second NAMES first.tool second.tool PATHS data/one data/two NO_DEFAULT_PATH NAMES_PER_DIR)
message("${first}|${second}")
