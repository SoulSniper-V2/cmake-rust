configure_file(data/input configured NEWLINE_STYLE CR)
file(READ configured out HEX)
message("[${out}]")
