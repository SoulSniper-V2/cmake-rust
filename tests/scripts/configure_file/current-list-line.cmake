file(WRITE input [=[@CMAKE_CURRENT_LIST_LINE@=${CMAKE_CURRENT_LIST_LINE}]=])
configure_file(input output)
file(READ output content HEX)
message("${content}")
