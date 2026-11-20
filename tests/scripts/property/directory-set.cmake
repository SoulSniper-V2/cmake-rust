set_directory_properties(PROPERTIES custom "x;y")
get_directory_property(x custom)
message("${x}")
