execute_process(COMMAND /bin/sh -c "printf one >&2; printf x" COMMAND /bin/sh -c "cat; printf two >&2" ERROR_VARIABLE err OUTPUT_VARIABLE out)
message("${out}|${err}")
