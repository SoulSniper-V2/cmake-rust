execute_process(COMMAND /bin/sh -c "kill -TERM $$" RESULT_VARIABLE x)
message("${x}")
