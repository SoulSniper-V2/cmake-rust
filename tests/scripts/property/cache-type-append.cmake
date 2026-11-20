set(needle x CACHE STRING "h")
set_property(CACHE needle APPEND PROPERTY TYPE PATH)
get_property(x CACHE needle PROPERTY TYPE)
message("${x}")
