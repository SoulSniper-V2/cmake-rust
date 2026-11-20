file(COPY_FILE data/prog prog)
file(RPATH_REMOVE FILE prog)
file(READ_ELF prog RPATH out)
message(STATUS "rpath=[${out}]")
