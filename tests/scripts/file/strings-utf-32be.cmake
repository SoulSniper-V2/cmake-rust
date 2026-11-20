file(STRINGS data/input auto)
file(STRINGS data/input explicit ENCODING UTF-32BE)
message("[${auto}][${explicit}]")
