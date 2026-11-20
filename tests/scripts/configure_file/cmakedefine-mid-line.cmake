set(DEF OK)
configure_file(data/input configured )
file(READ configured out HEX)
message("[${out}]")
