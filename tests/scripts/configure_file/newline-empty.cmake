configure_file(data/input configured NEWLINE_STYLE CRLF)
file(READ configured out HEX)
message("[${out}]")
