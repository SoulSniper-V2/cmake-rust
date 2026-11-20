execute_process(COMMAND /bin/sh -c "exit 7" COMMAND /bin/true RESULT_VARIABLE last RESULTS_VARIABLE all)
message("${last}|${all}")
