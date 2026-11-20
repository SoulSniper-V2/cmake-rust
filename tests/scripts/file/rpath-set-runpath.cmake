file(COPY_FILE data/runp prog)
file(RPATH_SET FILE prog NEW_RPATH "/s")
file(READ_ELF prog RUNPATH out)
message(STATUS "runpath=[${out}]")
