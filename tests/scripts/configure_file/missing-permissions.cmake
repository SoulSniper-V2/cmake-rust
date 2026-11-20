configure_file(data/input configured FILE_PERMISSIONS)
file(READ configured out HEX)
message("[${out}]")
