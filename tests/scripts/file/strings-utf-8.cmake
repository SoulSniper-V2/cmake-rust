file(STRINGS data/input auto)
file(STRINGS data/input explicit ENCODING UTF-8)
message("[${auto}][${explicit}]")
