execute_process(COMMAND /bin/false COMMAND /bin/true COMMAND_ERROR_IS_FATAL ANY)
message(FATAL_ERROR unreachable)
