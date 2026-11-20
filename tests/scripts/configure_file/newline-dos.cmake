configure_file(data/input configured NEWLINE_STYLE DOS)
file(READ configured out HEX)
message("[${out}]")
