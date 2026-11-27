file(MAKE_DIRECTORY "${CMAKE_CURRENT_LIST_DIR}/badbin")
file(WRITE "${CMAKE_CURRENT_LIST_DIR}/badbin/junk.txt" "junk")
ctest_empty_binary_directory("${CMAKE_CURRENT_LIST_DIR}/badbin")
