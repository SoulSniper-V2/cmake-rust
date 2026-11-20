set_property(SOURCE ./f.c PROPERTY custom a)
get_property(x SOURCE f.c PROPERTY custom)
message("${x}")
