execute_process(COMMAND /bin/sh -c "printf a; printf b >&2; printf c" OUTPUT_VARIABLE data ERROR_VARIABLE data)
message("[${data}]")
