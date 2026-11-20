set(VAR [=[a"b\c]=])
configure_file(data/input configured ESCAPE_QUOTES)
file(READ configured out HEX)
message("[${out}]")
