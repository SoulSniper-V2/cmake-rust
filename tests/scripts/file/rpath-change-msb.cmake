file(COPY_FILE data/msb prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/second/lib" NEW_RPATH "/m")
file(READ_ELF prog RPATH out)
message(STATUS "rpath=[${out}]")
