ctest_run_script("${CMAKE_CURRENT_LIST_DIR}/child.cmake" RETURN_VALUE rv)
message("rv=${rv} child=${CHILD_VAR}")
