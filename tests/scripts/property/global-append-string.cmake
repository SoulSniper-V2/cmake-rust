set_property(GLOBAL PROPERTY k a)
set_property(GLOBAL APPEND_STRING PROPERTY k b c)
get_property(x GLOBAL PROPERTY k)
message("[${x}]")
