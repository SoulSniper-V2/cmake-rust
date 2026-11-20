execute_process(COMMAND /bin/sh -c "printf data > input")
execute_process(COMMAND /bin/cat INPUT_FILE input OUTPUT_FILE output)
execute_process(COMMAND /bin/cat output OUTPUT_VARIABLE x)
message("${x}")
