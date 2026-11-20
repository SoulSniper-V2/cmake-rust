execute_process(COMMAND /bin/sh -c "printf hello" OUTPUT_VARIABLE out ECHO_OUTPUT_VARIABLE)
message("[${out}]")
