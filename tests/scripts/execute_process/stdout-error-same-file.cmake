execute_process(COMMAND /bin/sh -c "printf a; printf b >&2" OUTPUT_FILE merge ERROR_FILE merge)
execute_process(COMMAND /bin/cat merge OUTPUT_VARIABLE x)
message("${x}")
