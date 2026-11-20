execute_process(COMMAND /bin/cat INPUT_FILE absent OUTPUT_VARIABLE x RESULT_VARIABLE r)
message("[${x}] ${r}")
