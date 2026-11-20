set(needle "a;b")
get_property(x VARIABLE PROPERTY needle)
message("${x}")
