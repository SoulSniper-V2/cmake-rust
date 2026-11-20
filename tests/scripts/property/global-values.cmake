set_property(GLOBAL PROPERTY k a "b;c" "" d)
get_property(x GLOBAL PROPERTY k)
message("[${x}]")
