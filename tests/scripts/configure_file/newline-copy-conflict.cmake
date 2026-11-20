configure_file(data/input configured COPYONLY NEWLINE_STYLE LF)
file(READ configured out HEX)
message("[${out}]")
