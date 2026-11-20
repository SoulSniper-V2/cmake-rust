execute_process(COMMAND /bin/true WORKING_DIRECTORY /cmake-test-missing-dir RESULT_VARIABLE x)
message("${x}")
