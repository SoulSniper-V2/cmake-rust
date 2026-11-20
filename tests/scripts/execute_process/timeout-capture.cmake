execute_process(COMMAND /bin/sleep 1 TIMEOUT 0.05 RESULT_VARIABLE a RESULTS_VARIABLE b)
message("${a}|${b}")
