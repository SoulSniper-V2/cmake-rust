execute_process(COMMAND /bin/sh -c "printf a; printf b >&2" OUTPUT_VARIABLE data ERROR_VARIABLE data OUTPUT_QUIET)
message("[${data}]")
