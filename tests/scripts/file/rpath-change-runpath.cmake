file(COPY_FILE data/runp prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/second/lib" NEW_RPATH "/x")
file(READ_ELF prog RUNPATH out)
message(STATUS "runpath=[${out}]")
