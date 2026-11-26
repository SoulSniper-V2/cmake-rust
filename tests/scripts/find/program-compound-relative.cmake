cmake_minimum_required(VERSION 4.1)
# A name with a slash is first tried as given: CollapseFullPath with an empty
# base keeps it relative, so the debug log lists it unchanged.
set(CMAKE_FIND_DEBUG_MODE TRUE)
find_program(result NAMES missing-prog nosuch/missing-prog ../nosuch/./missing-prog PATHS /nonexistent-dir NO_DEFAULT_PATH)
message("${result}")
