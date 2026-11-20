configure_file(data/input configured NEWLINE_STYLE WIN32)
file(READ configured out HEX)
message("[${out}]")
