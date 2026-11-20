set(VAR yes)
configure_file(data/input configured )
file(READ configured out HEX)
message("[${out}]")
