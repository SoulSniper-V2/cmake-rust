file(COPY_FILE data/prog prog)
file(RPATH_SET FILE prog NEW_RPATH "")
file(READ_ELF prog RPATH out)
message(STATUS "rpath=[${out}]")
