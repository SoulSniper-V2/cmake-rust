configure_file(data/input configured NEWLINE_STYLE UNIX)
file(READ configured out HEX)
message("[${out}]")
