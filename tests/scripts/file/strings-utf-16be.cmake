file(STRINGS data/input auto)
file(STRINGS data/input explicit ENCODING UTF-16BE)
message("[${auto}][${explicit}]")
