file(COPY_FILE data/e32 prog)
file(RPATH_REMOVE FILE prog)
file(READ_ELF prog RPATH out)
message(STATUS "rpath=[${out}]")
