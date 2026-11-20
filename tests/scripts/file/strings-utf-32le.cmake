file(STRINGS data/input auto)
file(STRINGS data/input explicit ENCODING UTF-32LE)
message("[${auto}][${explicit}]")
