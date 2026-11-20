file(COPY_FILE data/runp prog)
file(RPATH_REMOVE FILE prog)
file(READ_ELF prog RUNPATH out)
message(STATUS "runpath=[${out}]")
