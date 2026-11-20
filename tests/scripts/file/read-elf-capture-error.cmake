file(READ_ELF data/plain RPATH rp CAPTURE_ERROR err)
message(STATUS "err=[${err}]")
