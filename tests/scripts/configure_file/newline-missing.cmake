configure_file(data/input configured NEWLINE_STYLE)
file(READ configured out HEX)
message("[${out}]")
