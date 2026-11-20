file(COPY_FILE data/prog prog)
file(RPATH_CHANGE FILE prog OLD_RPATH "/second/lib" NEW_RPATH "/new/lib")
file(READ_ELF prog RPATH out)
message(STATUS "rpath=[${out}]")
