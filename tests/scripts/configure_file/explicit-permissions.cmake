configure_file(data/input configured FILE_PERMISSIONS OWNER_READ GROUP_READ)
file(READ configured out HEX)
message("[${out}]")
