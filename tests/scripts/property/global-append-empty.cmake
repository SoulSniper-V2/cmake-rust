set_property(GLOBAL APPEND PROPERTY k "")
get_property(x GLOBAL PROPERTY k SET)
message("set=${x}")
