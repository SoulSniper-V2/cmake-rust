file(MAKE_DIRECTORY target)
configure_file(data/input target)
file(READ target/input out HEX)
message("[${out}]")
