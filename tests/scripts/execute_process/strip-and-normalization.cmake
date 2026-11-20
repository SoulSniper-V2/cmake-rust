execute_process(COMMAND /usr/bin/printf [[a\000b\r\nc \t\n]] OUTPUT_VARIABLE x OUTPUT_STRIP_TRAILING_WHITESPACE)
message("[${x}]")
