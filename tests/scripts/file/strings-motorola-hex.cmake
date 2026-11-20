file(MAKE_DIRECTORY CMakeFiles)
file(STRINGS data/input decoded)
file(STRINGS data/input original NO_HEX_CONVERSION)
message("converted=[${decoded}] original=[${original}]")
