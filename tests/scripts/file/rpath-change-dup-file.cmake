file(COPY_FILE data/prog prog)
file(RPATH_CHANGE FILE bogus FILE prog OLD_RPATH "/first/lib" NEW_RPATH "/d")
file(READ_ELF prog RPATH out)
message(STATUS "rpath=[${out}]")
