execute_process(COMMAND /bin/true TIMEOUT -1 RESULT_VARIABLE r)
message("${r}")
