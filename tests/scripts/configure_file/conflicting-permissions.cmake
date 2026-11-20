configure_file(data/input configured USE_SOURCE_PERMISSIONS NO_SOURCE_PERMISSIONS)
file(READ configured out HEX)
message("[${out}]")
