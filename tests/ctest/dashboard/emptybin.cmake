file(MAKE_DIRECTORY "${CMAKE_CURRENT_LIST_DIR}/removebin")
file(WRITE "${CMAKE_CURRENT_LIST_DIR}/removebin/CMakeCache.txt" "")
file(WRITE "${CMAKE_CURRENT_LIST_DIR}/removebin/junk.txt" "junk")
ctest_empty_binary_directory("${CMAKE_CURRENT_LIST_DIR}/removebin")
