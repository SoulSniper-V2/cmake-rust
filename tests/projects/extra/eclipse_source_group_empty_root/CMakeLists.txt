cmake_minimum_required(VERSION 3.20)
project(empty_root_group C)
source_group([[\]] FILES root.h)
add_library(lib STATIC root.c root.h)
