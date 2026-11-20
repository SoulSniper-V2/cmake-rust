execute_process(COMMAND /bin/sh -c "printf hello >&2" ERROR_VARIABLE out ECHO_ERROR_VARIABLE)
message("[${out}]")
