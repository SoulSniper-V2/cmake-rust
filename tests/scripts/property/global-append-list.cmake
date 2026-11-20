set_property(GLOBAL PROPERTY k a "")
set_property(GLOBAL APPEND PROPERTY k b "")
get_property(x GLOBAL PROPERTY k)
message("[${x}]")
