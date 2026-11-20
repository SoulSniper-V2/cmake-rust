file(READ_ELF data/trunc RPATH out CAPTURE_ERROR err)
message(STATUS "err=[${err}]")
