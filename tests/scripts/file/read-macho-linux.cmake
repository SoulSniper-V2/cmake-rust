# Off Apple, READ_MACHO validates its arguments and the file, then reports
# that Mach-O support is not available (or stores that in CAPTURE_ERROR).
file(READ_MACHO "${CMAKE_CURRENT_LIST_FILE}" ARCHITECTURES a CAPTURE_ERROR e)
message("a=[${a}] e=[${e}]")
file(READ_MACHO /no/such/file ARCHITECTURES a2 CAPTURE_ERROR e2)
message("a2=[${a2}] e2=[${e2}]")
file(READ_MACHO /no/such/file CAPTURE_ERROR e3 CAPTURE_ERROR e4)
message("e3=[${e3}] e4=[${e4}]")
file(READ_MACHO "${CMAKE_CURRENT_LIST_FILE}" ARCHITECTURES x)
message("not reached")
