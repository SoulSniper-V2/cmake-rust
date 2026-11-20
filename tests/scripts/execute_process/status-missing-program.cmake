execute_process(COMMAND program-that-does-not-exist RESULT_VARIABLE x)
message("${x}")
