execute_process(COMMAND /bin/echo apple COMMAND /usr/bin/tr a A OUTPUT_VARIABLE x OUTPUT_STRIP_TRAILING_WHITESPACE)
message("[${x}]")
