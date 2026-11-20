execute_process(COMMAND /bin/sh -c "printf a; printf b >&2" OUTPUT_VARIABLE out ERROR_VARIABLE err ERROR_QUIET)
message("[${out}]|[${err}]")
