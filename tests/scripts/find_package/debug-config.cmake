cmake_minimum_required(VERSION 4.1)
set(CMAKE_FIND_DEBUG_MODE ON)
find_package(Acme CONFIG HINTS /nonexistent NO_DEFAULT_PATH QUIET)
