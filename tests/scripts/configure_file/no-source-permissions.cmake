configure_file(data/input configured NO_SOURCE_PERMISSIONS)
file(READ configured out HEX)
message("[${out}]")
