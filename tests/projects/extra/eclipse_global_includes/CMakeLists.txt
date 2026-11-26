cmake_minimum_required(VERSION 3.20)
project(gi C)
# Every directory's global targets (install, edit_cache, ...) take part in
# the .cproject include paths, so a directory holding only an INTERFACE
# library still contributes its current dirs and include_directories().
set(CMAKE_INCLUDE_CURRENT_DIR ON)
add_subdirectory(cfg)
add_subdirectory(src)
install(FILES CMakeLists.txt DESTINATION share)
