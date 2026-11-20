set(VAR "a;b")
configure_file(data/input configured @ONLY)
file(READ configured out HEX)
message("[${out}]")
