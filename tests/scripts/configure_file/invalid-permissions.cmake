configure_file(data/input configured FILE_PERMISSIONS OWNER_READ OOPS WHAT)
file(READ configured out HEX)
message("[${out}]")
