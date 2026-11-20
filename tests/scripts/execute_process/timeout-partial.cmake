execute_process(COMMAND /bin/sh -c "printf beginning; exec /bin/sleep 1" TIMEOUT 0.1 OUTPUT_VARIABLE x RESULT_VARIABLE result)
message("${x}|${result}")
