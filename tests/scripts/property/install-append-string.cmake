set_property(INSTALL bin/f PROPERTY k "")
set_property(INSTALL bin/f APPEND_STRING PROPERTY k second)
get_property(x INSTALL bin/f PROPERTY k)
message("[${x}]")
