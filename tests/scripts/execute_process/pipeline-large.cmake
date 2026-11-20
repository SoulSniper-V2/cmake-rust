execute_process(COMMAND /usr/bin/seq 1 50000 COMMAND /usr/bin/wc -l OUTPUT_VARIABLE x OUTPUT_STRIP_TRAILING_WHITESPACE)
message("[${x}]")
