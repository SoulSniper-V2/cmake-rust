cmake_minimum_required(VERSION 4.1)
find_program(absolute "${CMAKE_CURRENT_SOURCE_DIR}/data/tool.tool" NO_DEFAULT_PATH)
find_program(relative data/tool.tool NO_DEFAULT_PATH)
message("${absolute}|${relative}")
