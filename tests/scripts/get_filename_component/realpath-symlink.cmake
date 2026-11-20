file(MAKE_DIRECTORY real)
file(CREATE_LINK real link SYMBOLIC)
get_filename_component(out link REALPATH)

message("${out}")
