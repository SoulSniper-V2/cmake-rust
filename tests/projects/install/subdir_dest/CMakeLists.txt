cmake_minimum_required(VERSION 3.30)
project(subdir_dest C)
set(CMAKE_INSTALL_PREFIX /prefix)
add_subdirectory(sub)
