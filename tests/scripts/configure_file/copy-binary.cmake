configure_file(data/input configured COPYONLY)
file(READ configured out HEX)
message("[${out}]")
