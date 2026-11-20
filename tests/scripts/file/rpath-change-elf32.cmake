file(COPY_FILE data/e32 prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/second/lib" NEW_RPATH "/3")
file(READ_ELF prog RPATH out)
message(STATUS "rpath=[${out}]")
