file(STRINGS data/input auto)
file(STRINGS data/input explicit ENCODING UTF-16LE)
message("[${auto}][${explicit}]")
