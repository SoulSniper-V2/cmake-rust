cmake_policy(SET CMP0153 OLD)
exec_program(/bin/echo ARGS hello OUTPUT_VARIABLE output RETURN_VALUE status)
message("${output}|${status}")
