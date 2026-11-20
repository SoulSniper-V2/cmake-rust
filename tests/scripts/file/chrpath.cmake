file(COPY_FILE data/prog prog)
file(CHRPATH FILE prog OLD_RPATH "/first/lib" NEW_RPATH "/c")
file(READ_ELF prog RPATH out)
message(STATUS "rpath=[${out}]")
