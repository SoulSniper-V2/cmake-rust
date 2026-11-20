file(COPY_FILE data/msb prog)
file(RPATH_REMOVE FILE prog)
file(READ_ELF prog RPATH out)
message(STATUS "rpath=[${out}]")
